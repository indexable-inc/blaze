//! The intrinsics table: the one home of what the device backend lowers by name rather than
//! through a Rust body (docs/future/gpu/shaders.md, "Intrinsics").
//!
//! Three kinds:
//!
//! - **Device types** ([`device_ty`]): the gpu crate's vectors become naga vectors, and its
//!   `Texture2d`, `Sampler` and `Tlas` become naga's image, sampler and acceleration structure
//!   types. A vector's fields are its
//!   components in order, which is what its `#[repr(C)]` layout already says; the layout itself
//!   still comes from the front end (`mirrors-derive-or-check`).
//! - **Device functions** ([`device_fn`]): the gpu crate's functions that only a GPU can run
//!   (crates/gpu/src/intrinsic.rs), whose host bodies panic naming themselves.
//! - **Core intrinsics** ([`core_op`]): the `core::intrinsics` that `f32`'s methods, the integer
//!   methods and `Ord`'s integer `min` and `max` are made of: a naga math function, an integer
//!   operation, a bitcast, a slice's length.
//! - **libm** ([`cmath`]): the C functions std's `f32` methods call where core has no intrinsic
//!   (`atan2` is `cmath::atan2f`), each one naga math function. std's own method bodies are still
//!   what is lowered; only the foreign call at their bottom is this table's.
//!
//! Everything else is lowered through its Rust body, so the host and the device run the same
//! code. A function with no body that is not here is refused (`why::NO_BODY`).
use base_db::Db;
use blaze_items::{ItemId, key};
use hir_def::{AdtId, FunctionId, ItemContainerId, signatures::FunctionSignature};
use hir_ty::{
    db::HirDatabase as _,
    intrinsic::{FloatWidth, Intrinsic},
    mir::BinOp,
    next_solver::TyKind,
};
use naga::{DerivativeAxis, MathFunction, Scalar, VectorSize};
use rustc_type_ir::inherent::{AdtDef as _, IntoKind as _};

/// A type the device has natively.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeviceTy {
    /// A naga vector; its Rust fields are the components, in order.
    Vector { scalar: Scalar, size: VectorSize },
    /// `gpu::Texture2d`: a sampled 2D texture of four floats.
    Texture2d,
    /// `gpu::Sampler`.
    Sampler,
    /// `gpu::Tlas`: a top-level acceleration structure.
    Tlas,
    /// `gpu::RawOut<T>`: the runtime-sized array of `T` a kernel's read-write storage holds.
    RawOut,
}

/// The device types, by their paths in the gpu crate.
const DEVICE_TYS: [(&str, DeviceTy); 12] = [
    ("gpu::vec::Vec2", DeviceTy::Vector { scalar: Scalar::F32, size: VectorSize::Bi }),
    ("gpu::vec::Vec3", DeviceTy::Vector { scalar: Scalar::F32, size: VectorSize::Tri }),
    ("gpu::vec::Vec4", DeviceTy::Vector { scalar: Scalar::F32, size: VectorSize::Quad }),
    ("gpu::vec::UVec2", DeviceTy::Vector { scalar: Scalar::U32, size: VectorSize::Bi }),
    ("gpu::vec::UVec3", DeviceTy::Vector { scalar: Scalar::U32, size: VectorSize::Tri }),
    ("gpu::vec::UVec4", DeviceTy::Vector { scalar: Scalar::U32, size: VectorSize::Quad }),
    ("gpu::vec::IVec2", DeviceTy::Vector { scalar: Scalar::I32, size: VectorSize::Bi }),
    ("gpu::vec::IVec4", DeviceTy::Vector { scalar: Scalar::I32, size: VectorSize::Quad }),
    ("gpu::entry::Texture2d", DeviceTy::Texture2d),
    ("gpu::entry::Sampler", DeviceTy::Sampler),
    ("gpu::ray::Tlas", DeviceTy::Tlas),
    ("gpu::entry::RawOut", DeviceTy::RawOut),
];

/// A function only the device runs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeviceFn {
    /// `dpdx`, `dpdy`, `fwidth`: naga's derivative on the axis.
    Derivative(DerivativeAxis),
    /// `discard()`: naga's `Kill`.
    Discard,
    /// `Texture2d::sample(&self, &Sampler, Vec2) -> Vec4`, its level from the quad's derivatives.
    Sample,
    /// `Texture2d::sample_level(&self, &Sampler, Vec2, f32) -> Vec4`.
    SampleLevel,
    /// `Texture2d::load(&self, IVec2, u32) -> Vec4`.
    Load,
    /// `RawOut::count(&self) -> usize`: its runtime-sized array's `arrayLength`.
    RawOutLen,
    /// `RawOut::load(&self, usize) -> T`.
    RawOutLoad,
    /// `RawOut::store(&self, usize, T)`.
    RawOutStore,
    /// `Tlas::query(&self, Ray, flags: u32, mask: u32) -> Committed`: one naga ray query,
    /// initialized, proceeded to its end, its committed intersection read.
    TlasQuery,
    /// `f32tof16(f32) -> u32`: naga's `pack2x16float` of `(x, 0)`.
    F32ToF16,
    /// `f16tof32(u32) -> f32`: the first of naga's `unpack2x16float`.
    F16ToF32,
}

impl DeviceFn {
    /// Whether only a fragment stage has it.
    pub fn fragment_only(self) -> bool {
        matches!(self, DeviceFn::Derivative(_) | DeviceFn::Discard | DeviceFn::Sample)
    }
}

/// The device functions, by their paths (an inherent method under its type's path).
const DEVICE_FNS: [(&str, DeviceFn); 13] = [
    ("gpu::intrinsic::dpdx", DeviceFn::Derivative(DerivativeAxis::X)),
    ("gpu::intrinsic::dpdy", DeviceFn::Derivative(DerivativeAxis::Y)),
    ("gpu::intrinsic::fwidth", DeviceFn::Derivative(DerivativeAxis::Width)),
    ("gpu::intrinsic::discard", DeviceFn::Discard),
    ("gpu::entry::Texture2d::sample", DeviceFn::Sample),
    ("gpu::entry::Texture2d::sample_level", DeviceFn::SampleLevel),
    ("gpu::entry::Texture2d::load", DeviceFn::Load),
    ("gpu::entry::RawOut::count", DeviceFn::RawOutLen),
    ("gpu::entry::RawOut::load", DeviceFn::RawOutLoad),
    ("gpu::entry::RawOut::store", DeviceFn::RawOutStore),
    ("gpu::ray::Tlas::query", DeviceFn::TlasQuery),
    // These two have host bodies that run as written; the device has an instruction for each.
    ("gpu::math::f32tof16", DeviceFn::F32ToF16),
    ("gpu::math::f16tof32", DeviceFn::F16ToF32),
];

/// The device type `adt` is, if it is one.
pub fn device_ty(db: &Db, adt: AdtId) -> Option<DeviceTy> {
    let path = key::path(db, ItemId::Adt(adt));
    DEVICE_TYS.iter().find(|(p, _)| *p == path).map(|(_, ty)| *ty)
}

/// The device function `func` is, if it is one.
pub fn device_fn(db: &Db, func: FunctionId) -> Option<DeviceFn> {
    let path = fn_path(db, func);
    DEVICE_FNS.iter().find(|(p, _)| *p == path).map(|(_, f)| *f)
}

/// A function's path as the table names it: a free function's item path, an inherent method's
/// under its self type's path (`gpu::entry::Texture2d::sample`), any other's item path.
pub fn fn_path(db: &Db, func: FunctionId) -> String {
    if let ItemContainerId::ImplId(impl_) = func.loc(db).container
        && db.impl_trait(impl_).is_none()
        && let TyKind::Adt(def, _) = db.impl_self_ty(impl_).skip_binder().kind()
    {
        let mut path = key::path(db, ItemId::Adt(def.def_id())); // alloc: a path, per call site
        path.push_str("::");
        path.push_str(FunctionSignature::of(db, func).name.as_str());
        return path;
    }
    key::path(db, ItemId::Function(func))
}

/// What a core intrinsic is on the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreOp {
    /// A naga math function, with how many arguments it takes.
    Math(MathFunction, usize),
    /// `round`: half away from zero, where naga's `round` (Metal's `rint`) is half to even.
    RoundHalfAway,
    /// An integer operation as MIR's operator of the same meaning: the wrapping ones, the
    /// unchecked ones (whose result past the range is UB, so wrapping is one of their meanings),
    /// and the `*_with_overflow` ones, which give `(result, overflowed)`.
    Bin(BinOp),
    /// `transmute` between types of one size: a bitcast.
    Bitcast,
    /// `float_to_int_unchecked`: a conversion (out of range is UB).
    FloatToInt,
    /// `ptr_metadata`: a slice pointer's length, a thin pointer's `()`.
    Metadata,
    /// `read_via_copy(p)`: the value `p` points at (`ptr::read`, `mem::replace`).
    Read,
    /// `write_via_move(p, v)`: `v` stored where `p` points.
    Write,
    /// `forget(v)`: nothing; the device drops nothing.
    Forget,
}

/// What a core intrinsic is on the device; `None` for the ones it does not have. Only `f32`
/// widths of the float ones: the device has no other float.
pub fn core_op(intrinsic: Intrinsic) -> Option<CoreOp> {
    let math = |fun, arity| Some(CoreOp::Math(fun, arity));
    match intrinsic {
        Intrinsic::Sqrt => math(MathFunction::Sqrt, 1),
        Intrinsic::Powf => math(MathFunction::Pow, 2),
        Intrinsic::Fabs => math(MathFunction::Abs, 1),
        Intrinsic::Floor => math(MathFunction::Floor, 1),
        Intrinsic::Ceil => math(MathFunction::Ceil, 1),
        Intrinsic::Trunc => math(MathFunction::Trunc, 1),
        Intrinsic::RoundTiesEven => math(MathFunction::Round, 1),
        Intrinsic::Round => Some(CoreOp::RoundHalfAway),
        Intrinsic::Exp => math(MathFunction::Exp, 1),
        Intrinsic::Exp2 => math(MathFunction::Exp2, 1),
        Intrinsic::Log => math(MathFunction::Log, 1),
        Intrinsic::Log2 => math(MathFunction::Log2, 1),
        Intrinsic::Sin => math(MathFunction::Sin, 1),
        Intrinsic::Cos => math(MathFunction::Cos, 1),
        Intrinsic::Fma | Intrinsic::Fmuladd => math(MathFunction::Fma, 3),
        Intrinsic::IntegerMin => math(MathFunction::Min, 2),
        Intrinsic::IntegerMax => math(MathFunction::Max, 2),
        Intrinsic::MinimumNumberNsz(FloatWidth::F32) => math(MathFunction::Min, 2),
        Intrinsic::MaximumNumberNsz(FloatWidth::F32) => math(MathFunction::Max, 2),
        Intrinsic::WrappingAdd | Intrinsic::UncheckedAdd => Some(CoreOp::Bin(BinOp::Add)),
        Intrinsic::WrappingSub | Intrinsic::UncheckedSub => Some(CoreOp::Bin(BinOp::Sub)),
        Intrinsic::WrappingMul | Intrinsic::UncheckedMul => Some(CoreOp::Bin(BinOp::Mul)),
        Intrinsic::UncheckedDiv | Intrinsic::ExactDiv => Some(CoreOp::Bin(BinOp::Div)),
        Intrinsic::UncheckedRem => Some(CoreOp::Bin(BinOp::Rem)),
        Intrinsic::UncheckedShl => Some(CoreOp::Bin(BinOp::Shl)),
        Intrinsic::UncheckedShr => Some(CoreOp::Bin(BinOp::Shr)),
        Intrinsic::AddWithOverflow => Some(CoreOp::Bin(BinOp::AddWithOverflow)),
        Intrinsic::SubWithOverflow => Some(CoreOp::Bin(BinOp::SubWithOverflow)),
        Intrinsic::MulWithOverflow => Some(CoreOp::Bin(BinOp::MulWithOverflow)),
        Intrinsic::Transmute | Intrinsic::TransmuteUnchecked => Some(CoreOp::Bitcast),
        Intrinsic::FloatToIntUnchecked => Some(CoreOp::FloatToInt),
        Intrinsic::PtrMetadata => Some(CoreOp::Metadata),
        Intrinsic::ReadViaCopy => Some(CoreOp::Read),
        Intrinsic::WriteViaMove => Some(CoreOp::Write),
        Intrinsic::Forget => Some(CoreOp::Forget),
        _ => None,
    }
}

/// The libm functions std's `f32` methods call, by their C names.
const CMATH: [(&str, MathFunction, usize); 8] = [
    ("atan2f", MathFunction::Atan2, 2),
    ("atanf", MathFunction::Atan, 1),
    ("asinf", MathFunction::Asin, 1),
    ("acosf", MathFunction::Acos, 1),
    ("tanf", MathFunction::Tan, 1),
    ("sinhf", MathFunction::Sinh, 1),
    ("coshf", MathFunction::Cosh, 1),
    ("tanhf", MathFunction::Tanh, 1),
];

/// The naga math function a foreign function of std's is, if it is one of [`CMATH`]: a function
/// in an `extern` block of the toolchain's std, by its C name.
pub fn cmath(db: &Db, func: FunctionId) -> Option<(MathFunction, usize)> {
    if !matches!(func.loc(db).container, ItemContainerId::ExternBlockId(_)) {
        return None;
    }
    if !key::path(db, ItemId::Function(func)).starts_with("std::") {
        return None;
    }
    let name = FunctionSignature::of(db, func).name.as_str().to_owned(); // alloc: a name, per call site
    CMATH.iter().find(|(c, ..)| *c == name).map(|(_, fun, arity)| (*fun, *arity))
}
