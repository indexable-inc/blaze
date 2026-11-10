//! Building an extension: what the host asks of the daemon's build, and what comes back.
//!
//! An extension is compiled the way a proc macro's crate is planned (its own unit, built for
//! the host, what it runs being build-time code), with three differences: its target is the
//! daemon's own triple and its backend the native one (stencils or LLVM, never Cranelift);
//! its code is metered (`blaze_codegen`'s `Spec::fuel`); and it is compiled against the
//! metadata of the API crates as this blaze's own build wrote them, and linked against the
//! running daemon's own image, so every type it shares with blaze has the layout blaze's code
//! reads it with and every non-generic function it calls is the daemon's one copy.
//!
//! That build lives in the daemon (the unit graph, the code generator, the linker), outside
//! this crate: [`Toolchain`] is the one call the host makes into it, and [`Image`] what it
//! hands back for [`crate::load::load`]. The implementation is the glue this slice does not
//! have (docs/v2/plan/design/extensions.md, "What slice 1 does not do").

use std::{any::Any, fmt};

use blaze_ids::ContentId;

use crate::source::Sources;

/// One extension to build.
pub struct Request<'s> {
    /// [`crate::key::build_key`]: what the image is kept under.
    pub key: ContentId,
    /// What to compile: checked ([`crate::check::check`]) before this is made.
    pub sources: &'s Sources,
    /// The function of the crate root whose address [`Image::entry`] is.
    pub entry: &'static str,
}

/// The entry every extension has: `pub fn extension() -> blaze_ext::Extension`.
pub const ENTRY: &str = "extension";

/// The daemon's build, as the host calls it.
pub trait Toolchain {
    /// Compiles `request.sources` natively and metered, or says which item could not be.
    fn build(&self, request: &Request<'_>) -> Result<Image, BuildError>;
}

/// The addresses an image's code calls the host at.
#[derive(Clone, Copy, Debug)]
pub struct Imports {
    /// `__blaze_fuel_cell`: [`blaze_ext::host::fuel_cell`].
    pub fuel_cell: usize,
    /// `__blaze_fuel_out`: [`blaze_ext::host::fuel_out`].
    pub fuel_out: usize,
}

/// What an image's placing keeps alive beside the code: its unwind tables' registration.
// dyn: one value per loaded extension, dropped with it
pub type Keep = Box<dyn Any>;

/// An extension's compiled code, not yet in memory.
pub struct Image {
    pub key: ContentId,
    pub code_len: usize,
    pub data_len: usize,
    /// Where in the code `extension` starts.
    pub entry: usize,
    /// Writes the code and the data at the addresses they will run at and resolves every
    /// relocation, the fuel runtime's against `Imports`; registers the code's unwind tables
    /// (a stop and a panic unwind through it) and gives back what keeps them registered.
    // dyn: called once per load
    pub place: Box<dyn FnOnce(&mut [u8], &mut [u8], &Imports) -> Result<Keep, String>>,
}

/// An extension the build refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildError {
    pub extension: String,
    /// The item and the reason, as the compiler says them.
    pub why: String,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "extension `{}` does not build: {}", self.extension, self.why)
    }
}

impl std::error::Error for BuildError {}
