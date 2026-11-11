//! What the device backend refuses, and why: each refusal names the item it is about (the entry
//! point, and the function the construct is in), the construct, and the rule that refused it
//! (`errors-name-the-thing`). Nothing here falls back: a construct the backend has no lowering
//! for stops the device build (`no-fallback`).
use std::fmt;

use base_db::{Crate, Db};
use hir_ty::{
    display::{DisplayTarget, HirDisplay as _},
    next_solver::Ty,
};

/// `ty` as a refusal names it: rendered with the database (`HirDisplay`), as a person reads it
/// in `krate`. Never `Debug`: a type's `Debug` reads a database only when one is attached, and
/// is not text for a person.
pub(crate) fn ty_name<'db>(db: &'db Db, krate: Crate, ty: Ty<'db>) -> String {
    ty.display(db, DisplayTarget::from_crate(db, krate)).to_string() // alloc: a refusal's text
}

/// One construct the backend does not lower.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The entry point being compiled, or the crate when no entry is reached yet.
    pub entry: String,
    /// The function the construct is in, by its path.
    pub item: String,
    /// What was met, in words.
    pub construct: String,
    /// The rule that refused it: one of [`why`].
    pub why: &'static str,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gpu entry `{}`: `{}`: {} is not lowered to the device: {}",
            self.entry, self.item, self.construct, self.why
        )
    }
}

/// What a device build fails with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Constructs the backend does not lower, every one met.
    Refused(Vec<Refusal>),
    /// The crate has no `GPU_ENTRIES` at its root, or it is not the gpu crate's `Entries`.
    NoEntries { krate: String, why: String },
    /// The front end could not give what the backend reads (MIR, a layout, a constant).
    Frontend { item: String, what: String },
    /// naga's validator refused the module the lowering made: a bug in the lowering, or a layout
    /// that a device address space can't hold, which the message names.
    Invalid { entry: String, what: String },
    /// naga's MSL writer refused the module.
    Msl(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Refused(all) => {
                for (i, refusal) in all.iter().enumerate() {
                    if i > 0 {
                        f.write_str("\n")?;
                    }
                    write!(f, "{refusal}")?;
                }
                Ok(())
            }
            Error::NoEntries { krate, why } => write!(f, "device build of `{krate}`: {why}"),
            Error::Frontend { item, what } => write!(f, "device build: `{item}`: {what}"),
            Error::Invalid { entry, what } => {
                write!(f, "device build: naga refused the module of gpu entry `{entry}`: {what}")
            }
            Error::Msl(what) => write!(f, "device build: naga's MSL writer: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// The rules a refusal cites, each where docs/future/gpu/shaders.md says it.
pub mod why {
    /// Only scalars of 32 bits, `bool`, the gpu crate's vectors, structs, tuples and arrays of
    /// them cross into a shader.
    pub const TYPES: &str = "a shader's values are bool, 32-bit integers and floats, gpu::vec vectors, and \
                             structs, tuples and arrays of them (docs/future/gpu/shaders.md, \"What lowers\")";
    /// Integers wider or narrower than 32 bits; `usize` is 32 bits on the device target.
    pub const WIDTH: &str = "a device integer is 32 bits wide: u8, u16, u64, u128 and their signed kin have no \
                             naga type, and usize is u32 on the device target (docs/future/gpu/shaders.md, \
                             \"The device target\")";
    /// A function pointer, `dyn`, a closure through a pointer: what is called must be known.
    pub const CALLS: &str = "every call in a shader is to a function known at compile time, inlined into its \
                             entry; a function pointer or `dyn` call has no lowering (docs/future/gpu/shaders.md, \
                             \"Calls\")";
    /// Recursion: an entry is one inlined body.
    pub const RECURSION: &str = "a shader is inlined into its entry, so nothing it calls may call itself \
                                 (docs/future/gpu/shaders.md, \"Calls\")";
    /// A reference whose target differs by path, or a raw pointer.
    pub const POINTERS: &str = "a reference in a shader names one place on every path (a local, a field or element \
                                of one, or a resource); raw pointers and references chosen at run time have no \
                                lowering (docs/future/gpu/shaders.md, \"References\")";
    /// An operation without a device lowering.
    pub const OPERATIONS: &str = "this operation has no device lowering (docs/future/gpu/shaders.md, \"What lowers\")";
    /// A call into a function without a body the backend can inline and no intrinsic.
    pub const NO_BODY: &str = "the function has no body to inline and is not in the intrinsics table \
                               (blaze-shader's intrinsics.rs, docs/future/gpu/shaders.md, \"Intrinsics\")";
    /// Control flow that is not reducible, or that unwinds.
    pub const CONTROL: &str = "a shader's control flow is reducible and never unwinds: a panic ends the invocation \
                               (docs/future/gpu/shaders.md, \"Panics\")";
    /// Enums: values inside a shader lower, with a layout of the device's own; none crosses
    /// into a resource, and a constant's bytes must be one of its variants.
    pub const ENUMS: &str = "an enum lowers as a value inside a shader, with the device's own layout, so no \
                             uniform or storage buffer may hold one (docs/future/gpu/shaders.md, \"Phase 2 as \
                             built\")";
    /// A statics read: a shader's only memory outside its frame is its resources.
    pub const STATICS: &str = "a shader reads no statics: its memory is its parameters' resources \
                               (docs/future/gpu/shaders.md, \"Resources\")";
    /// The entry's interface: what `GPU_ENTRIES` says against the function's signature.
    pub const INTERFACE: &str = "an entry's parameters and result are what its `EntryDesc` says, built by \
                                 #[gpu::vertex] and #[gpu::fragment] (crates/gpu/src/entry.rs)";
    /// An intrinsic used outside the stage that has it.
    pub const STAGE: &str = "derivatives, implicit-level sampling and discard exist only in a fragment stage \
                             (docs/future/gpu/shaders.md, \"Intrinsics\")";
}
