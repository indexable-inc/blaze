//! The ABI a fn is declared with, from its HEAD alone: one home for the rule, read by the
//! table's rows (tabled.rs `row`) and by the builder's question for a fn the program does not
//! table ([`FrontEnd::fn_abi`] on [`Front2Program`]); and whether the ABI unwinds
//! ([`BuildAbi::nounwind`], the table's answer to [`FrontEnd::nounwind`]).
//!
//! An ABI is a fact of a fn's signature, never of its body: the Reference, "Functions":
//! "Functions may qualify their signatures with an ABI: `extern "C" fn ..`", and "External
//! blocks": "Functions within external blocks ... the ABI of the block". So a fn whose body
//! the program leaves out (a provided method whose default body does not check, called only
//! through a vtable) still has its ABI, and a dynamic call through `&dyn Tr`, which names the
//! trait's fn and never its body (the Reference, "Trait objects"), is built with it
//! (tests/front2_callee_left_out.rs `a_dynamic_call_needs_no_body_of_the_method`).

use super::*;

/// A fn's written ABI as the build reads it: blaze-build's calling convention ([`Abi`]) AND
/// the ABI's unwind category, which blaze-build's one [`Abi::C`] does not carry (it stands for
/// "C", "C-unwind", "system" and "system-unwind" alike: one calling convention).
///
/// The unwind rule, from the Reference ("Functions", [items.fn.extern.unwind.behavior]): "The
/// \"unwinding\" ABI category refers to \"Rust\" (the implicit ABI of Rust functions not marked
/// extern), \"C-unwind\", and any other ABI with -unwind in its name. The \"non-unwinding\" ABI
/// category refers to all other ABI strings, including \"C\" and \"stdcall\"." Its table, which
/// covers a "function declaration or definition using the corresponding ABI string": with
/// "panic=unwind", a panic-unwind reaching an "unwinding" boundary is "unwind", a
/// "non-unwinding" one "abort". So [`BuildAbi::nounwind`] is ONE fact for both sides of a
/// boundary: for a fn defined in Rust, a panic reaching its edge aborts (blaze-build's
/// `Adts::nounwind`, read by walk/nounwind.rs `mark_nounwind`); for a foreign declaration, a
/// call of it never unwinds. The library's `#[rustc_nounwind]` makes a fn of the Rust ABI
/// nounwind too (core/src/panicking.rs:88-92): tabled.rs `row` ORs it in from the fn's
/// attributes (program.rs `symbols_of`).
///
/// The calling conventions ("External blocks", [items.extern.abi.unwind]): "extern
/// \"C-unwind\" and extern \"system-unwind\" — Identical to \"C\" and \"system\", respectively,
/// but with different behavior when the callee unwinds"; [items.extern.abi.system]: "This is
/// equivalent to extern \"C\" except on Windows x86_32", and Windows is not a host blaze builds
/// for (AGENTS.md "Not now"), so all four are [`Abi::C`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct BuildAbi {
    /// The calling convention.
    pub(super) abi: Abi,
    /// Whether the ABI is of the Reference's "non-unwinding" category.
    pub(super) nounwind: bool,
}

/// The ABI fn `key`'s head declares, as the builder reads it ([`BuildAbi`]). A foreign fn's
/// own text carries no ABI: its block's does, read alone (a module-level block's from the
/// names library, a block in a body's from the store: front2/panic_entry.rs `local_abi`). A
/// constructor is a safe Rust fn. Refused by name: no head ([`Front2Refusal::NoHead`]), an
/// unplaced head ([`Front2Refusal::Unplaced`]), an ABI with no reading
/// ([`Front2Refusal::Abi`]). `function` names the fn in the refusal.
/// Generic over the names source: the lazy program over records asks it too (lazy/fill.rs
/// `row`, program/seam_rules.rs `fn_abi`).
pub(super) fn head_abi<'n, S: crate::front2::source::NamesSource<'n>>(names: S, store: &Store<'n>, key: ItemKey, function: impl Fn() -> Box<str>) -> Result<BuildAbi, Front2Refusal> {
    if store.ctors.get(&key).is_some() {
        return Ok(BuildAbi { abi: Abi::Rust, nounwind: false });
    }
    let Some(head) = store.fns.get(&key) else {
        return Err(Front2Refusal::NoHead { function: function() });
    };
    let (_, foreign) = placed_fn(names, store, key, head.lib).map_err(|_| Front2Refusal::Unplaced { function: function() })?;
    let foreign = foreign.or_else(|| crate::front2::panic_entry::local_abi(store, key));
    let read = match foreign {
        None => of_head(head.header.abi),
        Some(block) => of_block(block),
    };
    read.ok_or_else(|| Front2Refusal::Abi { function: function(), abi: head.header.abi })
}

/// [`placed`] of fn `key` (library key `lib`), the one rule every row and ABI reads it by: a
/// fn declared in a BODY (its head is the store's local head, locals.rs) is no item the names
/// place, so it is at `{0, 0}` (owed, as a ctor) and no module-level `extern` block declares it;
/// a block in its body is its local head's (front2/panic_entry.rs `local_abi`, which every
/// caller asks next). Asked of the names source, a local's key is no row of the items record
/// (the solved names answer "none declares it", the records no answer), so it is never asked.
pub(super) fn placed_fn<'n, S: crate::front2::source::NamesSource<'n>>(names: S, store: &Store<'n>, key: ItemKey, lib: LibKey) -> Result<(At, Option<&'n str>), Unplaced> {
    if store.locals.head(key).is_some() {
        return Ok((At { file: 0, place: 0 }, None));
    }
    placed(names, lib)
}

/// Where constructor `key` is placed, the one rule the eager program (program.rs `make_from`)
/// and the lazy one (lazy/fill.rs `row`) table it by: a tuple variant's constructor at its
/// variant's text ([`item_at`]); a tuple struct's, a member key with no library key, at `{0, 0}`
/// (owed, placed with its struct). No `extern` block is asked: the Reference ("Enumerations",
/// "Structs") makes a constructor of the variant or struct itself, never a declaration of an
/// external block, so its ABI is the Rust ABI ([`head_abi`]) with no question to a names source.
pub(super) fn placed_ctor<'n, S: crate::front2::source::NamesSource<'n>>(names: S, lib: Option<LibKey>) -> At {
    lib.map_or(At { file: 0, place: 0 }, |lib| item_at(names, lib))
}

/// The ABI a fn's own head is written with (front2 heads.rs `abi_of`, read by types.rs
/// `fn_ptr_abi`). Exhaustive: a new ABI in the checker's [`tc::Abi`] stops this match from
/// compiling instead of being guessed. `None`: no builder ABI (refused by the caller).
pub(super) fn of_head(abi: tc::Abi) -> Option<BuildAbi> {
    match abi {
        tc::Abi::Rust => Some(BuildAbi { abi: Abi::Rust, nounwind: false }),
        tc::Abi::RustCall => Some(BuildAbi { abi: Abi::RustCall, nounwind: false }),
        tc::Abi::C => Some(BuildAbi { abi: Abi::C, nounwind: true }),
        tc::Abi::CUnwind => Some(BuildAbi { abi: Abi::C, nounwind: false }),
        tc::Abi::System => Some(BuildAbi { abi: Abi::C, nounwind: true }),
        tc::Abi::SystemUnwind => Some(BuildAbi { abi: Abi::C, nounwind: false }),
        // No builder ABI: a declaration of an `extern "llvm-intrinsic"` block is routed by
        // tabled.rs `row` before its ABI is asked (a target intrinsic: front2/llvm_intrinsic.rs);
        // a fn DEFINED with this ABI, or one left out whose ABI the builder asks, is refused.
        tc::Abi::LlvmIntrinsic => None,
        // Every other ABI of the nightly is a type the checker carries; the builder emits none
        // of them: refused by name by the caller.
        tc::Abi::Swift
        | tc::Abi::Aapcs
        | tc::Abi::AapcsUnwind
        | tc::Abi::AvrInterrupt
        | tc::Abi::AvrNonBlockingInterrupt
        | tc::Abi::Cdecl
        | tc::Abi::CdeclUnwind
        | tc::Abi::CmseNonsecureCall
        | tc::Abi::CmseNonsecureEntry
        | tc::Abi::Custom
        | tc::Abi::EfiApi
        | tc::Abi::Fastcall
        | tc::Abi::FastcallUnwind
        | tc::Abi::GpuKernel
        | tc::Abi::Msp430Interrupt
        | tc::Abi::PtxKernel
        | tc::Abi::RiscvInterruptM
        | tc::Abi::RiscvInterruptS
        | tc::Abi::RustCold
        | tc::Abi::RustInvalid
        | tc::Abi::RustPreserveNone
        | tc::Abi::Stdcall
        | tc::Abi::StdcallUnwind
        | tc::Abi::SysV64
        | tc::Abi::SysV64Unwind
        | tc::Abi::RustTail
        | tc::Abi::Thiscall
        | tc::Abi::ThiscallUnwind
        | tc::Abi::Vectorcall
        | tc::Abi::VectorcallUnwind
        | tc::Abi::Win64
        | tc::Abi::Win64Unwind
        | tc::Abi::X86Interrupt => None,
    }
}

/// The ABI of an `extern` block, as program.rs `placed` gives it (unquoted; a bare `extern` is
/// "C"): the ABI of every fn the block declares. `None` for any other string: the caller
/// refuses the fn by name ([`Front2Refusal::Abi`]).
pub(super) fn of_block(written: &str) -> Option<BuildAbi> {
    match written {
        "C" => Some(BuildAbi { abi: Abi::C, nounwind: true }),
        "C-unwind" => Some(BuildAbi { abi: Abi::C, nounwind: false }),
        "system" => Some(BuildAbi { abi: Abi::C, nounwind: true }),
        "system-unwind" => Some(BuildAbi { abi: Abi::C, nounwind: false }),
        "Rust" => Some(BuildAbi { abi: Abi::Rust, nounwind: false }),
        "rust-call" => Some(BuildAbi { abi: Abi::RustCall, nounwind: false }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{BuildAbi, of_block, of_head};
    use crate::front2::types::fn_ptr_abi;
    use blaze_build::Abi;
    use blaze_surface::Range;

    /// `extern "C"` and `extern "C-unwind"` are one calling convention but differ in unwinding,
    /// for a fn defined in Rust (its head's ABI, read from the written string as heads.rs
    /// `abi_of` reads it) and for a foreign declaration (its block's ABI).
    ///
    /// The Reference ("Functions", the unwinding table): "The \"unwinding\" ABI category refers
    /// to \"Rust\" ..., \"C-unwind\", and any other ABI with -unwind in its name. The
    /// \"non-unwinding\" ABI category refers to all other ABI strings, including \"C\""; with
    /// "panic=unwind", a panic reaching a non-unwinding boundary is "abort" (the table of
    /// [items.fn.extern.unwind.behavior], which covers a "function declaration or definition").
    /// [items.extern.abi.unwind]: "extern \"C-unwind\" and extern \"system-unwind\" — Identical
    /// to \"C\" and \"system\", respectively, but with different behavior when the callee
    /// unwinds".
    ///
    /// A fix that kept mapping both to one answer fails the first two assertions; one that
    /// treated the Rust ABI as non-unwinding fails the `"Rust"` lines; one that guessed an
    /// unknown block ABI fails the last.
    ///
    /// Expected: `test front2::program::head_abi::tests::c_and_c_unwind_differ_for_a_definition_and_a_declaration ... ok`
    #[test]
    fn c_and_c_unwind_differ_for_a_definition_and_a_declaration() {
        let r = Range::new(0, 0);
        let defined = |written: &str| of_head(fn_ptr_abi(written, r).ok().expect("an ABI the head accepts"));
        // A definition: `extern "C" fn f() {}` aborts at its edge; `extern "C-unwind"` unwinds.
        assert_eq!(defined("\"C\""), Some(BuildAbi { abi: Abi::C, nounwind: true }));
        assert_eq!(defined("\"C-unwind\""), Some(BuildAbi { abi: Abi::C, nounwind: false }));
        assert_eq!(defined("\"system\""), Some(BuildAbi { abi: Abi::C, nounwind: true }));
        assert_eq!(defined("\"system-unwind\""), Some(BuildAbi { abi: Abi::C, nounwind: false }));
        assert_eq!(defined("\"Rust\""), Some(BuildAbi { abi: Abi::Rust, nounwind: false }));
        assert_eq!(defined("\"rust-call\""), Some(BuildAbi { abi: Abi::RustCall, nounwind: false }));
        // A declaration: a call of a foreign "C" fn never unwinds; of a "C-unwind" one, may.
        assert_eq!(of_block("C"), Some(BuildAbi { abi: Abi::C, nounwind: true }));
        assert_eq!(of_block("C-unwind"), Some(BuildAbi { abi: Abi::C, nounwind: false }));
        assert_eq!(of_block("system"), Some(BuildAbi { abi: Abi::C, nounwind: true }));
        assert_eq!(of_block("system-unwind"), Some(BuildAbi { abi: Abi::C, nounwind: false }));
        assert_eq!(of_block("Rust"), Some(BuildAbi { abi: Abi::Rust, nounwind: false }));
        // An ABI the build has no reading of is refused by its caller, never guessed.
        assert_eq!(of_block("stdcall"), None);
        std::println!("head_abi: \"C\" aborts, \"C-unwind\" unwinds, for a definition and a declaration");
    }
}
