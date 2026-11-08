//! The intrinsics table: the one home of what the device backend lowers by name rather than
//! through a Rust body (docs/future/gpu/shaders.md, "Intrinsics").
//!
//! Three kinds:
//!
//! - **Device types** ([`device_ty`]): the gpu crate's vectors become naga vectors, and its
//!   `Texture2d` and `Sampler` become naga's image and sampler types. A vector's fields are its
//!   components in order, which is what its `#[repr(C)]` layout already says; the layout itself
//!   still comes from the front end (`mirrors-derive-or-check`).
//! - **Device functions** ([`device_fn`]): the gpu crate's functions that only a GPU can run
//!   (crates/gpu/src/intrinsic.rs), whose host bodies panic naming themselves.
//! - **Core intrinsics** ([`core_math`]): the `core::intrinsics` that `f32`'s methods and `Ord`'s
//!   integer `min` and `max` are made of, each one naga math function.
//!
//! Everything else is lowered through its Rust body, so the host and the device run the same
//! code. A function with no body that is not here is refused (`why::NO_BODY`).
use base_db::Db;
use blaze_items::{ItemId, key};
use hir_def::{AdtId, FunctionId, ItemContainerId, signatures::FunctionSignature};
use hir_ty::{
    db::HirDatabase as _,
    intrinsic::{FloatWidth, Intrinsic},
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
}

/// The device types, by their paths in the gpu crate.
const DEVICE_TYS: [(&str, DeviceTy); 7] = [
    ("gpu::vec::Vec2", DeviceTy::Vector { scalar: Scalar::F32, size: VectorSize::Bi }),
    ("gpu::vec::Vec3", DeviceTy::Vector { scalar: Scalar::F32, size: VectorSize::Tri }),
    ("gpu::vec::Vec4", DeviceTy::Vector { scalar: Scalar::F32, size: VectorSize::Quad }),
    ("gpu::vec::UVec4", DeviceTy::Vector { scalar: Scalar::U32, size: VectorSize::Quad }),
    ("gpu::vec::IVec2", DeviceTy::Vector { scalar: Scalar::I32, size: VectorSize::Bi }),
    ("gpu::entry::Texture2d", DeviceTy::Texture2d),
    ("gpu::entry::Sampler", DeviceTy::Sampler),
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
}

impl DeviceFn {
    /// Whether only a fragment stage has it.
    pub fn fragment_only(self) -> bool {
        matches!(self, DeviceFn::Derivative(_) | DeviceFn::Discard | DeviceFn::Sample)
    }
}

/// The device functions, by their paths (an inherent method under its type's path).
const DEVICE_FNS: [(&str, DeviceFn); 7] = [
    ("gpu::intrinsic::dpdx", DeviceFn::Derivative(DerivativeAxis::X)),
    ("gpu::intrinsic::dpdy", DeviceFn::Derivative(DerivativeAxis::Y)),
    ("gpu::intrinsic::fwidth", DeviceFn::Derivative(DerivativeAxis::Width)),
    ("gpu::intrinsic::discard", DeviceFn::Discard),
    ("gpu::entry::Texture2d::sample", DeviceFn::Sample),
    ("gpu::entry::Texture2d::sample_level", DeviceFn::SampleLevel),
    ("gpu::entry::Texture2d::load", DeviceFn::Load),
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

/// The naga math function a core intrinsic is, with how many arguments it takes; `None` for the
/// intrinsics the device does not have. Only `f32` widths: the device has no other float.
///
/// `round` is not here: Rust's rounds half away from zero, naga's (Metal's `rint`) half to
/// even, so `round_ties_even` is the one that maps.
pub fn core_math(intrinsic: Intrinsic) -> Option<(MathFunction, usize)> {
    Some(match intrinsic {
        Intrinsic::Sqrt => (MathFunction::Sqrt, 1),
        Intrinsic::Powf => (MathFunction::Pow, 2),
        Intrinsic::Fabs => (MathFunction::Abs, 1),
        Intrinsic::Floor => (MathFunction::Floor, 1),
        Intrinsic::Ceil => (MathFunction::Ceil, 1),
        Intrinsic::Trunc => (MathFunction::Trunc, 1),
        Intrinsic::RoundTiesEven => (MathFunction::Round, 1),
        Intrinsic::Exp => (MathFunction::Exp, 1),
        Intrinsic::Exp2 => (MathFunction::Exp2, 1),
        Intrinsic::Log => (MathFunction::Log, 1),
        Intrinsic::Log2 => (MathFunction::Log2, 1),
        Intrinsic::Sin => (MathFunction::Sin, 1),
        Intrinsic::Cos => (MathFunction::Cos, 1),
        Intrinsic::Fma | Intrinsic::Fmuladd => (MathFunction::Fma, 3),
        Intrinsic::IntegerMin => (MathFunction::Min, 2),
        Intrinsic::IntegerMax => (MathFunction::Max, 2),
        Intrinsic::MinimumNumberNsz(FloatWidth::F32) => (MathFunction::Min, 2),
        Intrinsic::MaximumNumberNsz(FloatWidth::F32) => (MathFunction::Max, 2),
        _ => return None,
    })
}
