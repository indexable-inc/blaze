//! The program's data as a run asks for it: what a reference that is not a direct call names
//! (a static, a thread-local, a vtable as a [`Datum`]; a function by its [`BodyId`]), and a
//! datum's initial bytes with the addresses in it. Answered by the same [`super::Bodies`] that
//! answers bodies, through its defaulted methods, so a provider that answers no data still
//! works and a run reaching data is refused by name.
//!
//! The form is blaze-build's (`crates/blaze-build/src/data.rs`, "The form": "Each allocation is
//! its bytes ..., its alignment, whether it may be written ..., and its relocations"), with one
//! difference: a relocation's addend is a field here, as in LIR's own `data_relocs`, and the
//! bytes under it are not read (blaze-build keeps the addend in the word: its provider moves it).
//! A function's own data (`RefKind::Data`) is in its body and is never asked for.

use super::BodyId;

/// A datum of the program by identity: the content id of the static, thread-local or vtable
/// it is, or any 32 bytes the provider chooses. Only the provider reads it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DatumId(pub [u8; 32]);

/// What a reference, or a relocation, names.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Target {
    /// A datum: a static's, a thread-local's or a vtable's bytes.
    Datum(DatumId),
    /// A function, by the identity [`super::Bodies::body`] answers its body for. A function
    /// outside the program (a C function) has an identity too; it has no body.
    Func(BodyId),
}

/// An address in a datum: at byte `offset`, a pointer-sized word that is the address of
/// `target` plus `addend`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DatumReloc {
    pub offset: u32,
    pub target: Target,
    pub addend: i64,
}

/// A datum's initial bytes, aligned to `1 << align`, the addresses in them, and whether the
/// program may write it (a `static mut`, a static with interior mutability, a thread-local).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Datum<'a> {
    pub bytes: &'a [u8],
    pub align: u8,
    pub relocs: &'a [DatumReloc],
    pub writable: bool,
}
