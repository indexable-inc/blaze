//! The tests' one unsafe module: functions of both backends mapped executable, linked to each
//! other and to the host, and called (stencil.md, "Tests").
//!
//! [`Exec::link`] lays out every [`Unit`] (a stencil [`Code`] or Cranelift's [`Compiled`]) in
//! one executable mapping, with a writable one beside it for the GOT, the tier counter cells
//! and the data objects, resolves each relocation as a linker would, registers the units'
//! unwind information with the system unwinder, and makes a Cranelift trampoline per unit so
//! any signature can be called with [`DataValue`]s. Names resolve by namespace: [`FUNCS`]
//! (index `i` is unit `i`), [`DATA`] (the data objects), [`HOST`] (host functions by address),
//! libcalls through `dlsym` (and Cranelift's `__cranelift_x86_pshufb`, which no library has),
//! [`Name::Own`] into the unit itself and [`Name::Counter`] to the unit's own two cells.
//!
//! A call that may trap runs in a forked child ([`Exec::call_forked`],
//! [`Exec::values_forked`]): a trap ends the child with the signal's number, which the parent
//! reads, so no signal handler is ever installed in the test process.
//!
//! Exceptions: [`throw_fn`] is a host function that raises an exception of this module's own
//! class with a tag and a value. Every unit's frame is registered with this module's
//! personality and an LSDA made from the unit's call sites and handlers, so the unwinder lands
//! at the handler of the tag (or the default) of a `try_call` in code of either backend, with
//! the value in payload register 0 and the tag in register 1.
//!
//! Unix on x86-64 and AArch64 only (Linux with libgcc's unwinder, macOS with `MAP_JIT` and
//! libunwind). Windows' code is compiled and checked by the tests that run no code.

#![allow(unsafe_code)]

use std::{
    cell::UnsafeCell,
    ffi::{CStr, c_int, c_void},
    ptr,
};

use blaze_inproc::exec::Executable;
use cranelift_codegen::{
    binemit::Reloc as Kind,
    data_value::DataValue,
    ir::{self, InstBuilder, MemFlagsData, types},
    isa::{self, TargetIsa},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

pub use super::interp::Outcome;
use super::host::{self, Compiled};
use crate::code::{self, Code, Name};

/// Namespace of user names that are the linked units, by index.
pub const FUNCS: u32 = 0;
/// Namespace of user names that are [`Link::data`]'s objects, by index.
pub const DATA: u32 = 1;
/// Namespace of user names that are [`Link::host`]'s functions, by index.
pub const HOST: u32 = 2;

/// One function to link: its code and the signature it is called with.
#[derive(Clone, Copy)]
pub enum Unit<'a> {
    Stencil(&'a Code, &'a ir::Signature),
    Cranelift(&'a Compiled, &'a ir::Signature),
}

/// What [`Exec::link`] links.
pub struct Link<'a> {
    /// The ISA the units are compiled for: the host's ([`host::isa`]). The trampolines and the
    /// unwind CIE are made with it.
    pub isa: &'a dyn TargetIsa,
    pub units: &'a [Unit<'a>],
    /// Each copied into writable memory, 16-byte aligned; read back with [`Exec::data`].
    pub data: &'a [&'a [u8]],
    /// Addresses of host functions (`extern "C"` or `extern "C-unwind"`), such as
    /// [`throw_fn`].
    pub host: &'a [usize],
}

/// A value an argument or result of [`Exec::call`] can be: what the C convention passes the
/// same way as the Cranelift type [`Arg::TY`].
pub trait Arg: Copy {
    const TY: ir::Type;
}

macro_rules! arg {
    ($($t:ty => $ty:expr),* $(,)?) => {
        $(impl Arg for $t { const TY: ir::Type = $ty; })*
    };
}

arg! {
    i8 => types::I8, u8 => types::I8, i16 => types::I16, u16 => types::I16,
    i32 => types::I32, u32 => types::I32, i64 => types::I64, u64 => types::I64,
    isize => types::I64, usize => types::I64, f32 => types::F32, f64 => types::F64,
}

impl<T> Arg for *const T {
    const TY: ir::Type = types::I64;
}

impl<T> Arg for *mut T {
    const TY: ir::Type = types::I64;
}

/// What [`Exec::call`] may return: nothing or one [`Arg`].
pub trait Ret: Copy {
    fn tys() -> Vec<ir::Type>;
}

impl Ret for () {
    fn tys() -> Vec<ir::Type> {
        Vec::new()
    }
}

impl<T: Arg> Ret for T {
    fn tys() -> Vec<ir::Type> {
        vec![T::TY]
    }
}

/// The arguments of [`Exec::call`]: a tuple of up to eight [`Arg`]s.
pub trait Args: Copy {
    fn tys() -> Vec<ir::Type>;

    /// Calls the code at `addr` with these arguments.
    ///
    /// # Safety
    ///
    /// `addr` is the entry of mapped executable code whose signature, in the host's C
    /// convention, takes exactly these arguments and returns `R`.
    #[doc(hidden)]
    unsafe fn invoke<R: Ret>(self, addr: usize) -> R;
}

macro_rules! args {
    ($($t:ident $v:ident),*) => {
        impl<$($t: Arg),*> Args for ($($t,)*) {
            fn tys() -> Vec<ir::Type> {
                vec![$($t::TY),*]
            }

            unsafe fn invoke<R: Ret>(self, addr: usize) -> R {
                let ($($v,)*) = self;
                // SAFETY: the caller's contract: `addr` is code of exactly this signature,
                // and a function pointer has the size and representation of an address.
                let f = unsafe {
                    std::mem::transmute::<usize, extern "C-unwind" fn($($t),*) -> R>(addr)
                };
                f($($v),*)
            }
        }
    };
}

args!();
args!(A a);
args!(A a, B b);
args!(A a, B b, C c);
args!(A a, B b, C c, D d);
args!(A a, B b, C c, D d, E e);
args!(A a, B b, C c, D d, E e, F f);
args!(A a, B b, C c, D d, E e, F f, G g);
args!(A a, B b, C c, D d, E e, F f, G g, H h);

/// One 16-byte place of a trampoline's argument or result array.
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct Slot([u8; 16]);

/// Linked, executable code of one or more units.
pub struct Exec {
    /// The code, then the GOT, the counter cells and the data objects.
    exec: Executable,
    /// Each unit's offset in the code, the linked units then their trampolines.
    at: Vec<usize>,
    sigs: Vec<ir::Signature>,
    units: usize,
    /// Each unit's trampoline's index in `at`, or why Cranelift could not make it.
    tramps: Vec<Result<usize, String>>,
    counters: usize,
    blobs: Vec<(usize, usize)>,
    /// Kept for as long as the frames are registered: the personality reads them.
    _lsdas: Vec<Box<[u32]>>,
    frames: Frames,
}

impl Exec {
    /// One stencil function, linked alone for the host.
    pub fn new(code: &Code, sig: &ir::Signature) -> Exec {
        let isa = host_isa();
        Exec::link(&Link { isa: &*isa, units: &[Unit::Stencil(code, sig)], data: &[], host: &[] })
    }

    /// One function of Cranelift's, linked alone for the host.
    pub fn cranelift(compiled: &Compiled, sig: &ir::Signature) -> Exec {
        let isa = host_isa();
        Exec::link(&Link {
            isa: &*isa,
            units: &[Unit::Cranelift(compiled, sig)],
            data: &[],
            host: &[],
        })
    }

    /// Links `link`'s units for the host. A name that resolves to nothing or a relocation out
    /// of range panics naming it: the tests link only what they made.
    pub fn link(link: &Link<'_>) -> Exec {
        let units = link.units.len();
        let tramp_sig = tramp_signature(link.isa);
        let tramp_code: Vec<Result<Compiled, String>> = link
            .units
            .iter()
            .enumerate()
            .map(|(i, unit)| {
                let sig = match unit {
                    Unit::Stencil(_, sig) | Unit::Cranelift(_, sig) => *sig,
                };
                trampoline(sig, i as u32, link.isa)
                    .and_then(|func| host::cranelift_caught(&func, link.isa))
            })
            .collect();
        let mut views: Vec<View<'_>> = link.units.iter().map(|unit| View::of(*unit)).collect();
        let mut tramps = Vec::with_capacity(units);
        for compiled in &tramp_code {
            match compiled {
                Ok(compiled) => {
                    tramps.push(Ok(views.len()));
                    views.push(View::of(Unit::Cranelift(compiled, &tramp_sig)));
                }
                Err(e) => tramps.push(Err(e.clone())),
            }
        }

        // The code: each unit at its alignment, then a veneer per call relocation.
        let mut at = Vec::with_capacity(views.len());
        let mut end = 0usize;
        let (mut calls, mut gots) = (0usize, 0usize);
        for view in &views {
            end = end.next_multiple_of(view.align.max(16) as usize);
            at.push(end);
            end += view.bytes.len();
            for reloc in view.relocs {
                match reloc.kind {
                    Kind::X86CallPCRel4 | Kind::X86CallPLTRel4 | Kind::Arm64Call => calls += 1,
                    Kind::X86GOTPCRel4 | Kind::Aarch64AdrGotPage21 | Kind::Aarch64Ld64GotLo12Nc => {
                        gots += 1
                    }
                    _ => {}
                }
            }
        }
        let veneers = end.next_multiple_of(16);
        let code_len = page(veneers + calls * VENEER);

        // The data: the GOT, then two counter cells per unit, then the objects.
        let counters = (gots * 8).next_multiple_of(16);
        let mut blobs = Vec::with_capacity(link.data.len());
        let mut data_end = counters + views.len() * 16;
        for blob in link.data {
            data_end = data_end.next_multiple_of(16);
            blobs.push((data_end, blob.len()));
            data_end += blob.len();
        }
        let data_len = page(data_end.max(1));

        let symbols = Symbols::new();
        // The units are written at the addresses they run at, so the relocations resolve as
        // the code is copied in; then `Executable` makes it executable.
        let write = |code_bytes: &mut [u8], data_bytes: &mut [u8]| {
            for (view, at) in views.iter().zip(&at) {
                code_bytes[*at..*at + view.bytes.len()].copy_from_slice(view.bytes);
            }
            for (blob, (at, len)) in link.data.iter().zip(&blobs) {
                data_bytes[*at..*at + *len].copy_from_slice(blob);
            }
            // The slices are not used again: the writer patches through these pointers.
            let (code, data) = (code_bytes.as_mut_ptr(), data_bytes.as_mut_ptr());
            let mut writer = Writer::new(code, data, veneers, calls, gots);
            for (i, view) in views.iter().enumerate() {
                for reloc in view.relocs {
                    let target = match reloc.name {
                        Name::User { namespace: FUNCS, index } => {
                            let index = index as usize;
                            assert!(index < units, "a call of unit {index} of {units}");
                            code as usize + at[index]
                        }
                        Name::User { namespace: DATA, index } => {
                            let (at, _) = blobs[index as usize];
                            data as usize + at
                        }
                        Name::User { namespace: HOST, index } => link.host[index as usize],
                        Name::User { namespace, index } => {
                            panic!("unit {i}: a relocation against u{namespace}:{index}, which no test makes")
                        }
                        Name::LibCall(call) => symbols.libcall(call),
                        Name::Known(symbol) => panic!("unit {i}: a relocation against {symbol:?}"),
                        Name::Own(offset) => code as usize + at[i] + offset as usize,
                        Name::Counter => data as usize + counters + i * 16,
                    };
                    let place = code as usize + at[i] + reloc.at as usize;
                    assert!(
                        reloc.at as usize + reloc_len(reloc.kind) <= view.bytes.len(),
                        "unit {i}: a {:?} at {} past its {} bytes",
                        reloc.kind,
                        reloc.at,
                        view.bytes.len()
                    );
                    writer.relocate(i, place, reloc.kind, target, reloc.addend);
                }
            }
        };
        let exec = Executable::written(code_len, data_len, write)
            .unwrap_or_else(|it| panic!("executable memory for the units: {}", it.what));
        let code_at = exec.code_address() as usize;

        let mut lsdas = Vec::with_capacity(views.len());
        for view in &views {
            lsdas.push(lsda(view.calls, view.handlers));
        }
        let frames = Frames::register(link.isa, &views, &at, code_at, &lsdas);
        let sigs = views.iter().map(|view| view.sig.clone()).collect();
        Exec { exec, at, sigs, units, tramps, counters, blobs, _lsdas: lsdas, frames }
    }

    fn code_at(&self) -> usize {
        self.exec.code_address() as usize
    }

    fn data_at(&self) -> usize {
        self.exec.data_address() as usize
    }

    /// The address of unit `unit`'s code, for a `call_indirect` or a function pointer.
    pub fn addr(&self, unit: usize) -> usize {
        assert!(unit < self.units, "unit {unit} of {}", self.units);
        self.code_at() + self.at[unit]
    }

    /// The address of data object `index`.
    pub fn data_addr(&self, index: usize) -> usize {
        self.data_at() + self.blobs[index].0
    }

    /// The bytes of data object `index` as the code left them.
    pub fn data(&self, index: usize) -> Vec<u8> {
        let (at, len) = self.blobs[index];
        let mut out = vec![0; len];
        // SAFETY: the object is `len` bytes at `at` in the live data mapping, and no call is
        // running (calls borrow `self` and have returned), so nothing writes it meanwhile.
        unsafe { ptr::copy_nonoverlapping((self.data_at() + at) as *const u8, out.as_mut_ptr(), len) };
        out
    }

    /// Unit `unit`'s tier counter cells: its calls and its back edges.
    pub fn counts(&self, unit: usize) -> [u64; 2] {
        assert!(unit < self.units, "unit {unit} of {}", self.units);
        let cell = self.data_at() + self.counters + unit * 16;
        // SAFETY: the cells are 16 bytes, 16-aligned, in the live data mapping; the code
        // writes them only during a call, and none is running.
        unsafe { [ptr::read_volatile(cell as *const u64), ptr::read_volatile((cell + 8) as *const u64)] }
    }

    /// Calls unit `unit` with `args`, in this process: for calls that must not trap. Panics
    /// when `A` and `R` are not the unit's signature in the host's C convention.
    pub fn call<A: Args, R: Ret>(&self, unit: usize, args: A) -> R {
        self.check_typed(unit, &A::tys(), &R::tys());
        // SAFETY: `check_typed` held the unit's signature to `A -> R` in the host's C
        // convention, and `addr` is its entry in the live executable mapping.
        unsafe { args.invoke::<R>(self.addr(unit)) }
    }

    /// [`Exec::call`] in a forked child, so a trap ends the child, not the tests.
    pub fn call_forked<A: Args, R: Ret>(&self, unit: usize, args: A) -> Outcome<R> {
        self.check_typed(unit, &A::tys(), &R::tys());
        let addr = self.addr(unit);
        let out = forked(std::mem::size_of::<R>(), |out| {
            // SAFETY: checked as in `call`; `out` is `size_of::<R>()` writable bytes.
            unsafe { ptr::write_unaligned(out.cast::<R>(), args.invoke::<R>(addr)) }
        });
        match out {
            Outcome::Returned(bytes) => {
                // SAFETY: the child wrote an `R` (an integer, a float, a pointer or nothing:
                // every bit pattern of it is valid) into these `size_of::<R>()` bytes.
                Outcome::Returned(unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<R>()) })
            }
            Outcome::Trapped(signal) => Outcome::Trapped(signal),
        }
    }

    /// Calls unit `unit` with `args` (one per parameter of its signature, a pointer as an
    /// `I64` for a struct argument or a return area) through its trampoline, in this process,
    /// and gives its results.
    pub fn values(&self, unit: usize, args: &[DataValue]) -> Vec<DataValue> {
        let (tramp, args, mut rets) = self.prepare(unit, args);
        // SAFETY: `tramp` is a trampoline Cranelift made for `(i64, i64)` in the host's C
        // convention; it reads one slot per parameter from `args` and writes one per result
        // to `rets`, each array as long as `prepare` made it.
        unsafe { (args.as_ptr(), rets.as_mut_ptr()).invoke::<()>(tramp) };
        self.results(unit, &rets)
    }

    /// [`Exec::values`] in a forked child.
    pub fn values_forked(&self, unit: usize, args: &[DataValue]) -> Outcome<Vec<DataValue>> {
        let (tramp, args, rets) = self.prepare(unit, args);
        let len = rets.len() * 16;
        let out = forked(len, |out| {
            // SAFETY: as in `values`; `out` is `len` writable bytes, 16-aligned (a fresh
            // mapping), one slot per result.
            unsafe { (args.as_ptr(), out.cast::<Slot>()).invoke::<()>(tramp) }
        });
        match out {
            Outcome::Returned(bytes) => {
                let rets: Vec<Slot> = bytes
                    .chunks_exact(16)
                    .map(|chunk| Slot(chunk.try_into().expect("16 bytes")))
                    .collect();
                Outcome::Returned(self.results(unit, &rets))
            }
            Outcome::Trapped(signal) => Outcome::Trapped(signal),
        }
    }

    fn check_typed(&self, unit: usize, params: &[ir::Type], rets: &[ir::Type]) {
        assert!(unit < self.units, "unit {unit} of {}", self.units);
        let sig = &self.sigs[unit];
        assert_eq!(sig.call_conv, host_call_conv(), "unit {unit}: not the host's C convention");
        let got: Vec<ir::Type> = sig.params.iter().map(|param| param.value_type).collect();
        assert!(
            sig.params.iter().all(|param| param.purpose == ir::ArgumentPurpose::Normal),
            "unit {unit}: {sig} has a special parameter; call it with `values`"
        );
        assert_eq!(got, params, "unit {unit}: the parameters of {sig}");
        let got: Vec<ir::Type> = sig.returns.iter().map(|ret| ret.value_type).collect();
        assert_eq!(got, rets, "unit {unit}: the returns of {sig}");
    }

    fn prepare(&self, unit: usize, args: &[DataValue]) -> (usize, Vec<Slot>, Vec<Slot>) {
        assert!(unit < self.units, "unit {unit} of {}", self.units);
        let tramp = match &self.tramps[unit] {
            Ok(tramp) => self.code_at() + self.at[*tramp],
            Err(e) => panic!("unit {unit}: no trampoline for {}: {e}", self.sigs[unit]),
        };
        let sig = &self.sigs[unit];
        assert_eq!(args.len(), sig.params.len(), "unit {unit}: the arguments of {sig}");
        let mut slots = vec![Slot::default(); args.len().max(1)];
        for (k, (arg, param)) in args.iter().zip(&sig.params).enumerate() {
            assert!(fits(arg, param.value_type), "unit {unit}: argument {k} {arg:?} of {sig}");
            arg.write_to_slice_le(&mut slots[k].0);
        }
        (tramp, slots, vec![Slot::default(); sig.returns.len().max(1)])
    }

    fn results(&self, unit: usize, rets: &[Slot]) -> Vec<DataValue> {
        self.sigs[unit]
            .returns
            .iter()
            .zip(rets)
            .map(|(ret, slot)| DataValue::read_from_slice_le(&slot.0, ret.value_type))
            .collect()
    }
}

impl Drop for Exec {
    fn drop(&mut self) {
        self.frames.deregister();
    }
}

/// Whether `value` is passed as a `ty`: its type, or any 128-bit vector for a 128-bit vector.
fn fits(value: &DataValue, ty: ir::Type) -> bool {
    if ty.is_vector() { value.ty().is_vector() && value.ty().bytes() == ty.bytes() } else { value.ty() == ty }
}

/// The address of a host function `(tag: i64, value: i64)` that raises an exception of this
/// module's class: a `try_call` with a handler for `tag` (or a default one) lands there with
/// `value` in payload register 0 and `tag` in register 1.
pub fn throw_fn() -> usize {
    throw as extern "C-unwind" fn(u64, u64) as usize
}

/// The host's ISA, as the tests compile for it.
pub fn host_isa() -> isa::OwnedTargetIsa {
    host::isa(&host::host().expect("the tests that run code run on a host target"))
}

fn host_call_conv() -> isa::CallConv {
    host::call_conv(&host::host().expect("a host target"))
}

/// The trampolines' signature: `(args: i64, rets: i64)` in the host's C convention.
fn tramp_signature(isa: &dyn TargetIsa) -> ir::Signature {
    let mut sig = ir::Signature::new(isa.default_call_conv());
    sig.params.push(ir::AbiParam::new(types::I64));
    sig.params.push(ir::AbiParam::new(types::I64));
    sig
}

/// A function of [`tramp_signature`] that loads each parameter of `sig` from its 16-byte slot
/// of `args`, calls unit `unit` (a [`FUNCS`] name), and stores each result to its slot of
/// `rets`.
fn trampoline(sig: &ir::Signature, unit: u32, isa: &dyn TargetIsa) -> Result<ir::Function, String> {
    let mut func = ir::Function::with_name_signature(
        ir::UserFuncName::user(u32::MAX, unit),
        tramp_signature(isa),
    );
    let mut cx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut cx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let (args, rets) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let callee_sig = b.import_signature(sig.clone());
    let name = b.func.declare_imported_user_function(ir::UserExternalName::new(FUNCS, unit));
    let callee = b.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(name),
        signature: callee_sig,
        colocated: false,
        patchable: false,
    });
    let mut values = Vec::with_capacity(sig.params.len());
    for (k, param) in sig.params.iter().enumerate() {
        values.push(b.ins().load(param.value_type, MemFlagsData::trusted(), args, (k * 16) as i32));
    }
    let call = b.ins().call(callee, &values);
    let results = b.inst_results(call).to_vec();
    for (k, value) in results.into_iter().enumerate() {
        b.ins().store(MemFlagsData::trusted(), value, rets, (k * 16) as i32);
    }
    b.ins().return_(&[]);
    b.seal_all_blocks();
    b.finalize(isa.frontend_config());
    Ok(func)
}

/// The bytes a relocation of `kind` writes.
fn reloc_len(kind: Kind) -> usize {
    match kind {
        Kind::Abs8 => 8,
        _ => 4,
    }
}

const VENEER: usize = 16;

fn page(len: usize) -> usize {
    // SAFETY: `sysconf` reads a constant of the system.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    len.max(1).next_multiple_of(page)
}

/// Writes code and relocations while the code is writable: inside `Executable::written`'s
/// `write`, at the addresses the code runs at.
struct Writer {
    code: *mut u8,
    data: *mut u8,
    veneers: usize,
    calls: usize,
    next_veneer: usize,
    gots: Vec<usize>,
    got_cap: usize,
}

impl Writer {
    fn new(code: *mut u8, data: *mut u8, veneers: usize, calls: usize, got_cap: usize) -> Writer {
        Writer {
            code,
            data,
            veneers,
            calls,
            next_veneer: 0,
            gots: Vec::with_capacity(got_cap),
            got_cap,
        }
    }

    /// The address of the GOT entry holding `target`, made on first use.
    fn got(&mut self, target: usize) -> usize {
        let index = match self.gots.iter().position(|it| *it == target) {
            Some(index) => index,
            None => {
                assert!(self.gots.len() < self.got_cap, "more GOT entries than GOT relocations");
                self.gots.push(target);
                let entry = self.data as usize + (self.gots.len() - 1) * 8;
                // SAFETY: the GOT is the first `got_cap * 8` bytes of the data, and
                // `entry` is 8-aligned within them.
                unsafe { ptr::write(entry as *mut u64, target as u64) };
                self.gots.len() - 1
            }
        };
        self.data as usize + index * 8
    }

    /// A veneer jumping to `target`, for a call it can't reach directly.
    fn veneer(&mut self, target: usize) -> usize {
        assert!(self.next_veneer < self.calls, "more veneers than call relocations");
        let veneer = self.code as usize + self.veneers + self.next_veneer * VENEER;
        self.next_veneer += 1;
        let bytes: [u8; 8] = if cfg!(target_arch = "x86_64") {
            // jmp qword [rip + 0]; the address follows.
            [0xff, 0x25, 0, 0, 0, 0, 0xcc, 0xcc]
        } else {
            // ldr x16, #8; br x16; the address follows.
            let [a, b, c, d] = 0x5800_0050u32.to_le_bytes();
            let [e, f, g, h] = 0xd61f_0200u32.to_le_bytes();
            [a, b, c, d, e, f, g, h]
        };
        let mut veneer_bytes = [0u8; VENEER];
        if cfg!(target_arch = "x86_64") {
            veneer_bytes[..6].copy_from_slice(&bytes[..6]);
            veneer_bytes[6..14].copy_from_slice(&(target as u64).to_le_bytes());
        } else {
            veneer_bytes[..8].copy_from_slice(&bytes);
            veneer_bytes[8..].copy_from_slice(&(target as u64).to_le_bytes());
        }
        // SAFETY: the veneer area holds `calls` veneers of `VENEER` bytes after `veneers`,
        // within the code (`code_len` counted them), writable while `write` runs.
        unsafe { ptr::copy_nonoverlapping(veneer_bytes.as_ptr(), veneer as *mut u8, VENEER) };
        veneer
    }

    /// Resolves one relocation of unit `unit` at `place` against `target` plus `addend`, as
    /// a static linker would (S + A - P), through a veneer or a GOT entry where the kind
    /// allows one.
    fn relocate(&mut self, unit: usize, place: usize, kind: Kind, target: usize, addend: i64) {
        let what = (target as i64).wrapping_add(addend);
        let pc = |to: i64| to.wrapping_sub(place as i64);
        match kind {
            Kind::Abs8 => self.put64(place, what as u64),
            Kind::Abs4 => {
                let value = u32::try_from(what)
                    .unwrap_or_else(|_| panic!("unit {unit}: Abs4 of {what:#x} out of range"));
                self.put32(place, value);
            }
            Kind::X86PCRel4 => self.put32(place, rel32(unit, kind, pc(what)) as u32),
            Kind::X86CallPCRel4 | Kind::X86CallPLTRel4 => {
                assert_eq!(addend, -4, "unit {unit}: a {kind:?} with addend {addend}");
                let disp = match i32::try_from(pc(what)) {
                    Ok(disp) => disp,
                    Err(_) => rel32(unit, kind, pc(self.veneer(target) as i64 + addend)),
                };
                self.put32(place, disp as u32);
            }
            Kind::X86GOTPCRel4 => {
                let got = self.got(target) as i64;
                self.put32(place, rel32(unit, kind, pc(got + addend)) as u32);
            }
            Kind::Arm64Call => {
                let mut delta = pc(what);
                if !(-(1 << 27)..1 << 27).contains(&delta) {
                    delta = pc(self.veneer(what as usize) as i64);
                }
                assert!(delta & 3 == 0, "unit {unit}: an unaligned call");
                let imm26 = ((delta >> 2) as u32) & 0x03ff_ffff;
                self.patch32(place, 0x03ff_ffff, imm26);
            }
            Kind::Aarch64AdrPrelPgHi21 => self.adrp(unit, place, what),
            Kind::Aarch64AdrGotPage21 => {
                let got = self.got(target) as i64 + addend;
                self.adrp(unit, place, got);
            }
            Kind::Aarch64AddAbsLo12Nc => {
                self.patch32(place, 0xfff << 10, ((what as u32) & 0xfff) << 10);
            }
            Kind::Aarch64Ld64GotLo12Nc => {
                let got = self.got(target) as i64 + addend;
                assert!(got & 7 == 0, "unit {unit}: an unaligned GOT entry");
                self.patch32(place, 0xfff << 10, (((got as u32) & 0xfff) >> 3) << 10);
            }
            other => panic!("unit {unit}: a {other:?} relocation, which the tests never make"),
        }
    }

    fn adrp(&mut self, unit: usize, place: usize, what: i64) {
        let delta = (what & !0xfff) - (place as i64 & !0xfff);
        assert!((-(1i64 << 32)..1i64 << 32).contains(&delta), "unit {unit}: adrp out of range");
        let pages = (delta >> 12) as u32;
        let lo = (pages & 3) << 29;
        let hi = ((pages >> 2) & 0x7_ffff) << 5;
        self.patch32(place, (3 << 29) | (0x7_ffff << 5), lo | hi);
    }

    fn put32(&mut self, place: usize, value: u32) {
        // SAFETY: `place` is a relocation's field, checked to be within its unit's bytes in
        // the code, writable while `write` runs.
        unsafe { ptr::write_unaligned(place as *mut u32, value) };
    }

    fn put64(&mut self, place: usize, value: u64) {
        // SAFETY: as in `put32`, for the 8 bytes `reloc_len` checked.
        unsafe { ptr::write_unaligned(place as *mut u64, value) };
    }

    fn patch32(&mut self, place: usize, mask: u32, bits: u32) {
        // SAFETY: as in `put32`.
        let inst = unsafe { ptr::read_unaligned(place as *const u32) };
        self.put32(place, (inst & !mask) | bits);
    }
}

fn rel32(unit: usize, kind: Kind, delta: i64) -> i32 {
    i32::try_from(delta).unwrap_or_else(|_| panic!("unit {unit}: {kind:?} of {delta:#x} out of range"))
}

/// Runs `f` in a forked child with `len` bytes of memory shared with this process, and gives
/// those bytes back when the child returns from `f`, or the signal that ended it.
fn forked(len: usize, f: impl FnOnce(*mut u8)) -> Outcome<Vec<u8>> {
    let shared_len = page(len);
    // SAFETY: a fresh anonymous mapping shared with the child, checked below.
    let shared = unsafe {
        libc::mmap(
            ptr::null_mut(),
            shared_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANON,
            -1,
            0,
        )
    };
    assert_ne!(shared, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
    // SAFETY: `fork` in a process with other threads: the child only runs `f` (mapped code
    // that allocates nothing and takes no lock the parent's threads could hold) and then
    // `_exit`s, and its handlers only `_exit`.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
    if pid == 0 {
        for signal in [libc::SIGILL, libc::SIGTRAP, libc::SIGFPE, libc::SIGSEGV, libc::SIGBUS] {
            // SAFETY: in the child: a handler that only `_exit`s, so a trap ends the child
            // with a status the parent reads, with no core dump or crash report.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_trap as extern "C" fn(c_int) as usize;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, ptr::null_mut());
            }
        }
        f(shared.cast());
        // SAFETY: ends the child without running the parent's destructors or atexit hooks.
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    loop {
        // SAFETY: waits for the child just forked.
        let done = unsafe { libc::waitpid(pid, &mut status, 0) };
        if done == pid {
            break;
        }
        let e = std::io::Error::last_os_error();
        assert_eq!(e.kind(), std::io::ErrorKind::Interrupted, "waitpid: {e}");
    }
    let out = if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        let mut out = vec![0; len];
        // SAFETY: the child wrote `len` bytes of the shared mapping and has ended.
        unsafe { ptr::copy_nonoverlapping(shared.cast::<u8>(), out.as_mut_ptr(), len) };
        Outcome::Returned(out)
    } else if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) > 128 {
        Outcome::Trapped(libc::WEXITSTATUS(status) - 128)
    } else if libc::WIFSIGNALED(status) {
        Outcome::Trapped(libc::WTERMSIG(status))
    } else {
        panic!("the child ended with status {status:#x}")
    };
    // SAFETY: the shared mapping made above, which nothing uses any more.
    unsafe { libc::munmap(shared, shared_len) };
    out
}

extern "C" fn on_trap(signal: c_int) {
    // SAFETY: `_exit` is async-signal-safe.
    unsafe { libc::_exit(128 + signal) }
}

/// Libcall addresses, from the process's own symbols.
struct Symbols {
    handle: *mut c_void,
}

impl Symbols {
    fn new() -> Symbols {
        // SAFETY: a handle to the program and every library it loaded; never closed.
        let handle = unsafe { libc::dlopen(ptr::null(), libc::RTLD_NOW) };
        assert!(!handle.is_null(), "dlopen of the test binary");
        Symbols { handle }
    }

    fn libcall(&self, call: ir::LibCall) -> usize {
        let name: &CStr = match call {
            ir::LibCall::CeilF32 => c"ceilf",
            ir::LibCall::CeilF64 => c"ceil",
            ir::LibCall::FloorF32 => c"floorf",
            ir::LibCall::FloorF64 => c"floor",
            ir::LibCall::TruncF32 => c"truncf",
            ir::LibCall::TruncF64 => c"trunc",
            ir::LibCall::NearestF32 => c"nearbyintf",
            ir::LibCall::NearestF64 => c"nearbyint",
            ir::LibCall::FmaF32 => c"fmaf",
            ir::LibCall::FmaF64 => c"fma",
            ir::LibCall::Memcpy => c"memcpy",
            ir::LibCall::Memset => c"memset",
            ir::LibCall::Memmove => c"memmove",
            ir::LibCall::Memcmp => c"memcmp",
            #[cfg(target_arch = "x86_64")]
            ir::LibCall::X86Pshufb => {
                return pshufb as extern "C" fn(Xmm, Xmm) -> Xmm as usize;
            }
            other => panic!("a call of {other:?}, which no test function makes"),
        };
        // SAFETY: a lookup of a NUL-terminated name in the handle from `dlopen`.
        let addr = unsafe { libc::dlsym(self.handle, name.as_ptr()) };
        assert!(!addr.is_null(), "no symbol {name:?} in the test binary");
        addr as usize
    }
}

#[cfg(target_arch = "x86_64")]
type Xmm = std::arch::x86_64::__m128i;

/// Cranelift's `__cranelift_x86_pshufb` (what `x86_pshufb` is without SSSE3): each byte of
/// `a` picked by the low four bits of `b`'s byte, or zero where its high bit is set.
#[cfg(target_arch = "x86_64")]
extern "C" fn pshufb(a: Xmm, b: Xmm) -> Xmm {
    // SAFETY: `__m128i` is 16 plain bytes, and every bit pattern of each is valid.
    let (a, b) = unsafe { (std::mem::transmute::<Xmm, [u8; 16]>(a), std::mem::transmute::<Xmm, [u8; 16]>(b)) };
    let mut out = [0u8; 16];
    for (out, index) in out.iter_mut().zip(b) {
        *out = if index & 0x80 != 0 { 0 } else { a[usize::from(index & 0xf)] };
    }
    // SAFETY: as above.
    unsafe { std::mem::transmute::<[u8; 16], Xmm>(out) }
}

/// One unit's parts, of either backend.
struct View<'a> {
    bytes: &'a [u8],
    align: u32,
    relocs: &'a [code::Reloc],
    calls: &'a [code::CallSite],
    handlers: &'a [code::Handler],
    unwind: Frame<'a>,
    sig: &'a ir::Signature,
}

enum Frame<'a> {
    None,
    Rows(u32, &'a [(u32, code::Rule)]),
    Cranelift(&'a isa::unwind::UnwindInfo),
}

impl<'a> View<'a> {
    fn of(unit: Unit<'a>) -> View<'a> {
        match unit {
            Unit::Stencil(code, sig) => View {
                bytes: &code.bytes,
                align: code.align,
                relocs: &code.relocs,
                calls: &code.calls,
                handlers: &code.handlers,
                unwind: match &code.unwind {
                    Some(code::Unwind::Dwarf { len, rows }) => Frame::Rows(*len, rows),
                    Some(code::Unwind::Windows { .. }) => panic!("Windows unwind info on a Unix host"),
                    Some(code::Unwind::WindowsArm64 { .. }) => panic!("Windows unwind info on a Unix host"),
                    None => Frame::None,
                },
                sig,
            },
            Unit::Cranelift(compiled, sig) => View {
                bytes: &compiled.bytes,
                align: compiled.align,
                relocs: &compiled.relocs,
                calls: &compiled.calls,
                handlers: &compiled.handlers,
                unwind: match &compiled.unwind {
                    Some(info) => Frame::Cranelift(info),
                    None => Frame::None,
                },
                sig,
            },
        }
    }
}

/// A unit's LSDA, as this module's personality reads it: the number of handlers, then per
/// handler `(return offset, 0 for a tag or 1 for the default, tag, landing offset)`, in the
/// order of each call's handlers.
fn lsda(calls: &[code::CallSite], handlers: &[code::Handler]) -> Box<[u32]> {
    let mut out = vec![0u32];
    for call in calls {
        for handler in &handlers[call.handlers.start as usize..call.handlers.end as usize] {
            match *handler {
                code::Handler::Tag { tag, landing } => out.extend([call.ret, 0, tag, landing]),
                code::Handler::Default { landing } => out.extend([call.ret, 1, 0, landing]),
            }
        }
    }
    out[0] = ((out.len() - 1) / 4) as u32;
    out.into_boxed_slice()
}

/// The units' frames, registered with the system unwinder for as long as the [`Exec`] lives.
struct Frames {
    eh_frame: Vec<u8>,
    /// Offsets in `eh_frame` of what was registered: the whole section with libgcc, each FDE
    /// with libunwind.
    registered: Vec<usize>,
}

unsafe extern "C" {
    fn __register_frame(begin: *const u8);
    fn __deregister_frame(begin: *const u8);
    fn _Unwind_RaiseException(exception: *mut Exception) -> c_int;
    fn _Unwind_GetLanguageSpecificData(context: *mut c_void) -> *const u8;
    fn _Unwind_GetRegionStart(context: *mut c_void) -> usize;
    fn _Unwind_GetIP(context: *mut c_void) -> usize;
    fn _Unwind_SetGR(context: *mut c_void, index: c_int, value: usize);
    fn _Unwind_SetIP(context: *mut c_void, value: usize);
}

impl Frames {
    fn register(
        isa: &dyn TargetIsa,
        views: &[View<'_>],
        at: &[usize],
        base: usize,
        lsdas: &[Box<[u32]>],
    ) -> Frames {
        use gimli::write::{Address, CallFrameInstruction as Cfi, EhFrame, EndianVec, FrameTable};
        let mut cie = isa.create_systemv_cie().expect("a CIE for the host's ISA");
        cie.fde_address_encoding = gimli::DW_EH_PE_absptr;
        cie.lsda_encoding = Some(gimli::DW_EH_PE_absptr);
        let personality = personality as unsafe extern "C" fn(c_int, c_int, u64, *mut Exception, *mut c_void) -> c_int;
        cie.personality = Some((gimli::DW_EH_PE_absptr, Address::Constant(personality as usize as u64)));
        let mut table = FrameTable::default();
        let cie = table.add_cie(cie);
        let mut any = false;
        for (i, view) in views.iter().enumerate() {
            let start = Address::Constant((base + at[i]) as u64);
            let mut fde = match &view.unwind {
                Frame::None => continue,
                Frame::Cranelift(isa::unwind::UnwindInfo::SystemV(info)) => info.to_fde(start),
                Frame::Cranelift(other) => panic!("unit {i}: {other:?} on a Unix host"),
                Frame::Rows(len, rows) => {
                    let mut fde = gimli::write::FrameDescriptionEntry::new(start, *len);
                    for (offset, rule) in rows.iter() {
                        let reg = gimli::Register;
                        let cfi = match *rule {
                            code::Rule::Cfa(r, off) => Cfi::Cfa(reg(r), off),
                            code::Rule::CfaRegister(r) => Cfi::CfaRegister(reg(r)),
                            code::Rule::CfaOffset(off) => Cfi::CfaOffset(off),
                            code::Rule::Offset(r, off) => Cfi::Offset(reg(r), off),
                            code::Rule::Restore(r) => Cfi::Restore(reg(r)),
                            code::Rule::SameValue(r) => Cfi::SameValue(reg(r)),
                            code::Rule::Undefined(r) => Cfi::Undefined(reg(r)),
                            code::Rule::RememberState => Cfi::RememberState,
                            code::Rule::RestoreState => Cfi::RestoreState,
                            code::Rule::ArgsSize(size) => Cfi::ArgsSize(size),
                            code::Rule::RaSignState(signed) => {
                                let mut expr = gimli::write::Expression::new();
                                expr.op(if signed { gimli::DW_OP_lit1 } else { gimli::DW_OP_lit0 });
                                Cfi::ValExpression(reg(34), expr)
                            }
                        };
                        fde.add_instruction(*offset, cfi);
                    }
                    fde
                }
            };
            fde.lsda = Some(Address::Constant(lsdas[i].as_ptr() as usize as u64));
            table.add_fde(cie, fde);
            any = true;
        }
        if !any {
            return Frames { eh_frame: Vec::new(), registered: Vec::new() };
        }
        let mut eh_frame = EhFrame(EndianVec::new(gimli::LittleEndian));
        table.write_eh_frame(&mut eh_frame).expect("eh_frame of the units");
        let mut eh_frame = eh_frame.0.into_vec();
        eh_frame.extend_from_slice(&[0; 4]);
        let mut registered = Vec::new();
        if cfg!(target_os = "macos") {
            // libunwind registers one FDE at a time: every entry whose CIE pointer is not 0.
            let mut at = 0;
            while at + 8 <= eh_frame.len() {
                let len = u32::from_le_bytes(eh_frame[at..at + 4].try_into().expect("4 bytes")) as usize;
                if len == 0 {
                    break;
                }
                let id = u32::from_le_bytes(eh_frame[at + 4..at + 8].try_into().expect("4 bytes"));
                if id != 0 {
                    registered.push(at);
                }
                at += 4 + len;
            }
        } else {
            registered.push(0);
        }
        for at in &registered {
            // SAFETY: `eh_frame` holds a well-formed section (gimli wrote it, terminated by a
            // zero length), its CIE before every FDE, and lives in this `Frames`, unmoved
            // (its heap buffer stays put) until `deregister`.
            unsafe { __register_frame(eh_frame.as_ptr().add(*at)) };
        }
        Frames { eh_frame, registered }
    }

    fn deregister(&mut self) {
        for at in self.registered.drain(..) {
            // SAFETY: exactly what `register` registered, from the same live buffer.
            unsafe { __deregister_frame(self.eh_frame.as_ptr().add(at)) };
        }
    }
}

/// `_Unwind_Exception` of the Itanium ABI, with this module's tag and value after it.
#[repr(C, align(16))]
struct Exception {
    class: u64,
    cleanup: Option<unsafe extern "C" fn(c_int, *mut Exception)>,
    private: [usize; 6],
    tag: u64,
    value: u64,
}

const CLASS: u64 = u64::from_be_bytes(*b"BLZTEST\0");

const URC_HANDLER_FOUND: c_int = 6;
const URC_INSTALL_CONTEXT: c_int = 7;
const URC_CONTINUE_UNWIND: c_int = 8;
const URC_FATAL_PHASE1_ERROR: c_int = 3;
const UA_SEARCH_PHASE: c_int = 1;
const UA_HANDLER_FRAME: c_int = 4;

thread_local! {
    static EXCEPTION: UnsafeCell<Exception> = const {
        UnsafeCell::new(Exception { class: CLASS, cleanup: None, private: [0; 6], tag: 0, value: 0 })
    };
}

extern "C-unwind" fn throw(tag: u64, value: u64) {
    let exception = EXCEPTION.with(|it| it.get());
    // SAFETY: this thread's own exception object, which nothing else refers to between
    // throws (a throw ends at its landing pad before the next begins on this thread).
    let code = unsafe {
        (*exception).tag = tag;
        (*exception).value = value;
        (*exception).private = [0; 6];
        _Unwind_RaiseException(exception)
    };
    eprintln!("no handler for the exception of tag {tag}: _Unwind_RaiseException gave {code}");
    std::process::abort();
}

/// The personality of every unit: finds the handler of the exception's tag (or the default)
/// for the call the frame is in, from the unit's [`lsda`].
unsafe extern "C" fn personality(
    version: c_int,
    actions: c_int,
    class: u64,
    exception: *mut Exception,
    context: *mut c_void,
) -> c_int {
    if version != 1 {
        return URC_FATAL_PHASE1_ERROR;
    }
    if class != CLASS {
        return URC_CONTINUE_UNWIND;
    }
    // SAFETY: the unwinder's context of a frame whose FDE `Frames::register` made: its LSDA
    // is a live `lsda` table (the `Exec` registering it is alive while its code runs).
    let (lsda, start, ip, tag, value) = unsafe {
        let lsda = _Unwind_GetLanguageSpecificData(context).cast::<u32>();
        if lsda.is_null() {
            return URC_CONTINUE_UNWIND;
        }
        let count = *lsda as usize;
        (
            std::slice::from_raw_parts(lsda.add(1), count * 4),
            _Unwind_GetRegionStart(context),
            _Unwind_GetIP(context),
            (*exception).tag,
            (*exception).value,
        )
    };
    let ret = (ip - start) as u32;
    let landing = lsda.chunks_exact(4).find_map(|row| {
        let matches = row[0] == ret && (row[1] == 1 || u64::from(row[2]) == tag);
        matches.then_some(row[3])
    });
    let Some(landing) = landing else {
        return URC_CONTINUE_UNWIND;
    };
    if actions & UA_SEARCH_PHASE != 0 {
        return URC_HANDLER_FOUND;
    }
    if actions & UA_HANDLER_FRAME == 0 {
        return URC_CONTINUE_UNWIND;
    }
    // SAFETY: the handler frame of the search phase: its payload registers (DWARF 0 and 1:
    // `rax`/`rdx`, `x0`/`x1`) and resume address are set as Cranelift's landing pads read
    // them.
    unsafe {
        _Unwind_SetGR(context, 0, value as usize);
        _Unwind_SetGR(context, 1, tag as usize);
        _Unwind_SetIP(context, start + landing as usize);
    }
    URC_INSTALL_CONTEXT
}

#[test]
fn a_cranelift_function_runs() {
    let isa = host_isa();
    let mut sig = ir::Signature::new(isa.default_call_conv());
    sig.params.push(ir::AbiParam::new(types::I64));
    sig.params.push(ir::AbiParam::new(types::I64));
    sig.returns.push(ir::AbiParam::new(types::I64));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(FUNCS, 0), sig.clone());
    let mut cx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut cx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let (x, y) = (b.block_params(entry)[0], b.block_params(entry)[1]);
    let sum = b.ins().iadd(x, y);
    b.ins().return_(&[sum]);
    b.seal_all_blocks();
    b.finalize(isa.frontend_config());
    let compiled = host::cranelift(&func, &*isa).expect("cranelift compiles an add");
    let exec = Exec::cranelift(&compiled, &sig);
    assert_eq!(exec.call::<(i64, i64), i64>(0, (40, 2)), 42);
    assert_eq!(exec.values(0, &[DataValue::I64(-1), DataValue::I64(1)]), [DataValue::I64(0)]);
    assert_eq!(exec.values_forked(0, &[DataValue::I64(7), DataValue::I64(8)]), Outcome::Returned(vec![DataValue::I64(15)]));
}

#[test]
fn a_trap_ends_only_the_child() {
    let isa = host_isa();
    let mut sig = ir::Signature::new(isa.default_call_conv());
    sig.params.push(ir::AbiParam::new(types::I32));
    sig.returns.push(ir::AbiParam::new(types::I32));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(FUNCS, 0), sig.clone());
    let mut cx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut cx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let x = b.block_params(entry)[0];
    let one = b.ins().iconst(types::I32, 100);
    let quotient = b.ins().udiv(one, x);
    b.ins().return_(&[quotient]);
    b.seal_all_blocks();
    b.finalize(isa.frontend_config());
    let compiled = host::cranelift(&func, &*isa).expect("cranelift compiles a divide");
    let exec = Exec::cranelift(&compiled, &sig);
    assert_eq!(exec.call_forked::<(i32,), i32>(0, (7,)), Outcome::Returned(14));
    assert!(exec.call_forked::<(i32,), i32>(0, (0,)).trapped());
}

#[test]
fn an_exception_lands_in_cranelift_code() {
    // u0:0 try-calls the host's throw and returns the payload its handler gets.
    let isa = host_isa();
    let mut sig = ir::Signature::new(isa.default_call_conv());
    sig.params.push(ir::AbiParam::new(types::I64));
    sig.returns.push(ir::AbiParam::new(types::I64));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(FUNCS, 0), sig.clone());
    let mut cx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut cx);
    let entry = b.create_block();
    let normal = b.create_block();
    let caught = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.append_block_param(caught, types::I64);
    b.append_block_param(caught, types::I64);
    b.switch_to_block(entry);
    let value = b.block_params(entry)[0];
    let mut throw_sig = ir::Signature::new(isa.default_call_conv());
    throw_sig.params.push(ir::AbiParam::new(types::I64));
    throw_sig.params.push(ir::AbiParam::new(types::I64));
    let throw_sig = b.import_signature(throw_sig);
    let name = b.func.declare_imported_user_function(ir::UserExternalName::new(HOST, 0));
    let throw_ref = b.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(name),
        signature: throw_sig,
        colocated: false,
        patchable: false,
    });
    let tag = b.ins().iconst(types::I64, 7);
    let normal_call = b.func.dfg.block_call(normal, &[]);
    let caught_call = b.func.dfg.block_call(
        caught,
        &[ir::BlockArg::TryCallExn(0), ir::BlockArg::TryCallExn(1)],
    );
    let table = b.func.dfg.exception_tables.push(ir::ExceptionTableData::new(
        throw_sig,
        normal_call,
        [ir::ExceptionTableItem::Tag(ir::ExceptionTag::from_u32(7), caught_call)],
    ));
    b.ins().try_call(throw_ref, &[tag, value], table);
    b.switch_to_block(normal);
    let zero = b.ins().iconst(types::I64, 0);
    b.ins().return_(&[zero]);
    b.switch_to_block(caught);
    let (payload, got_tag) = (b.block_params(caught)[0], b.block_params(caught)[1]);
    let sum = b.ins().iadd(payload, got_tag);
    b.ins().return_(&[sum]);
    b.seal_all_blocks();
    b.finalize(isa.frontend_config());
    let compiled = host::cranelift(&func, &*isa).expect("cranelift compiles a try_call");
    let exec = Exec::link(&Link {
        isa: &*isa,
        units: &[Unit::Cranelift(&compiled, &sig)],
        data: &[],
        host: &[throw_fn()],
    });
    assert_eq!(exec.call::<(i64,), i64>(0, (100,)), 107);
}
