//! The stencil libraries blaze was built with: one per [`Variant`], made by `build.rs` into
//! `OUT_DIR` and included in blaze's binary. [`Library::get`] checks one once and then reads
//! it in place: a lookup is [`format::Reader::find`], a stencil a borrow of its bytes.

pub mod format;
// The tree's pin and its parser, the ones the build script holds every library to: `Library::get`
// holds the included bytes to it again, so bytes put in `OUT_DIR` after the build script ran are
// refused, never used as a template key.
#[allow(dead_code, reason = "shared with build.rs and blaze-codegen-code; this reads the parser")]
#[path = "../pin.rs"]
mod pin;

#[cfg(test)]
mod tests;

use std::sync::OnceLock;

pub use format::{Hole, HoleKind, Stencil, Variant};

use crate::{Cpu, Error, Target, shape::Shape};

/// One checked library.
#[derive(Clone, Copy, Debug)]
pub struct Library {
    reader: format::Reader<'static>,
}

static X86_64: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/x86_64.stencils"));
static AARCH64: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/aarch64.stencils"));
static AARCH64_LSE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/aarch64_lse.stencils"));
/// The tree's pin, `stencils.json` (`pin.rs`).
static PIN: &str = include_str!("../stencils.json");

impl Variant {
    /// The library `target`'s code is made of: its architecture at the CPU baseline
    /// Cranelift's ISA is given for it.
    pub fn of(target: &Target) -> Result<Variant, Error> {
        match (target.arch, target.cpu) {
            (blaze_codegen::Arch::X86_64, Cpu::Baseline) => Ok(Variant::X86_64),
            (blaze_codegen::Arch::Aarch64, Cpu::Baseline) => Ok(Variant::Aarch64),
            (blaze_codegen::Arch::Aarch64, Cpu::AppleM1) => Ok(Variant::Aarch64Lse),
            // alloc: the error, which stops the compile
            (arch, cpu) => Err(Error::new(format!("no stencil library for {arch:?} at {cpu:?}"))),
        }
    }

    /// The library's bytes as built into blaze.
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Variant::X86_64 => X86_64,
            Variant::Aarch64 => AARCH64,
            Variant::Aarch64Lse => AARCH64_LSE,
        }
    }
}

impl Library {
    /// The library of `variant`, checked on first use: its structure, its variant, and that
    /// its bytes hash to the id the tree's pin names, which its header says too. A library
    /// that fails its check is an error naming why (a build of blaze gone wrong, or bytes
    /// changed after the build script checked them), every time it is asked for.
    pub fn get(variant: Variant) -> Result<&'static Library, Error> {
        static LIBRARIES: [OnceLock<Result<Library, String>>; 3] =
            [OnceLock::new(), OnceLock::new(), OnceLock::new()];
        let checked = LIBRARIES[variant as usize].get_or_init(|| {
            // alloc: once per library, the check's error
            let reader = format::Reader::new(variant.bytes()).map_err(|e| e.to_string())?;
            if reader.variant() != variant {
                // alloc: once per library, the check's error
                return Err(format!("the {:?} library says it is {:?}", variant, reader.variant()));
            }
            // alloc: once per library, the pin's entries and the check's error
            let pinned = pin::pin_parse(PIN).map_err(|e| format!("blaze-stencil's pin: {e}"))?;
            let Some(&(_, pinned)) = pinned.iter().find(|(name, _)| name == variant.file()) else {
                // alloc: once per library, the check's error
                return Err(format!("blaze-stencil's pin names no id for {}", variant.file()));
            };
            let (said, hashed) = (reader.id(), reader.hashed_id(blaze_ids::b3::hash));
            if hashed != pinned || said != pinned {
                // alloc: once per library, the check's error
                return Err(format!(
                    "{} is not the library blaze-stencil's pin names: the pinned id is {}, its \
                     bytes hash to {} and its header says {}; its bytes changed after the build \
                     script checked them",
                    variant.file(),
                    pin::pin_hex(&pinned),
                    pin::pin_hex(&hashed),
                    pin::pin_hex(&said)
                ));
            }
            Ok(Library { reader })
        });
        // alloc: the error, which stops the compile
        checked.as_ref().map_err(|e| Error::new(format!("the {variant:?} stencil library: {e}")))
    }

    pub fn id(&self) -> [u8; 32] {
        self.reader.id()
    }

    pub fn variant(&self) -> Variant {
        self.reader.variant()
    }

    /// The stencil of `shape`, if the set has it.
    #[inline]
    pub fn find(&self, shape: Shape) -> Option<Stencil<'static>> {
        self.reader.find(shape.key()).map(|index| self.reader.stencil(index))
    }

    pub fn reader(&self) -> &format::Reader<'static> {
        &self.reader
    }

    /// Every rodata entry, [`format::ENTRY`] bytes each.
    pub fn rodata(&self) -> &'static [u8] {
        self.reader.rodata()
    }

    /// The rodata entry a `Rodata` hole names ([`Hole::rodata`]).
    #[inline]
    pub fn entry(&self, entry: u32) -> &'static [u8] {
        self.reader.entry(entry)
    }
}
