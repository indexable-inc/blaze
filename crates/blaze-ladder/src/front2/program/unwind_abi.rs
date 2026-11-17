//! What the build reads of a fn's written ABI: blaze-build's calling convention ([`Abi`]) AND
//! the ABI's unwind category, which blaze-build's one [`Abi::C`] does not carry (it stands for
//! "C", "C-unwind", "system" and "system-unwind" alike: one calling convention).
//!
//! The rule, from the Reference (the pinned nightly's copy, "Functions",
//! [items.fn.extern.unwind.behavior]): "The table below indicates the behavior of an unwinding
//! operation reaching each type of ABI boundary (function declaration or definition using the
//! corresponding ABI string)." "The \"unwinding\" ABI category refers to \"Rust\" (the implicit
//! ABI of Rust functions not marked extern), \"C-unwind\", and any other ABI with -unwind in its
//! name. The \"non-unwinding\" ABI category refers to all other ABI strings, including \"C\" and
//! \"stdcall\"." The table: with "panic=unwind", a panic-unwind reaching an "unwinding" boundary
//! is "unwind", a "non-unwinding" one "abort"; a native unwind reaching a "non-unwinding"
//! boundary is "undefined behavior". So a panic reaching the edge of a "C" fn defined in Rust
//! aborts, and a call of a foreign "C" fn is assumed never to unwind; a "C-unwind" fn unwinds
//! either way.
//!
//! So [`BuildAbi::nounwind`] is ONE fact for both sides of a boundary: for a fn defined in Rust,
//! whether a panic that reaches its edge aborts (blaze-build's `Adts::nounwind`, read by
//! walk/nounwind.rs `mark_nounwind`); for a foreign declaration, that a call of it never unwinds.
//! The library's `#[rustc_nounwind]` makes a fn of the Rust ABI nounwind too (core/src/
//! panicking.rs:88-92, on `panic_nounwind_fmt`: "This attribute has the key side-effect that if
//! the panic handler ignores `can_unwind` and unwinds anyway, we will hit the \"unwinding out of
//! nounwind function\" guard, which causes a \"panic in a function that cannot unwind\""):
//! tabled.rs ORs it in from the fn's attributes (program.rs `symbols_of`).
//!
//! The calling conventions: "C-unwind" is "C" with unwinding allowed, the same registers and
//! layout ("External blocks", [items.extern.abi.unwind]: "extern \"C-unwind\" and extern
//! \"system-unwind\" — Identical to \"C\" and \"system\", respectively, but with different
//! behavior when the callee unwinds (by panicking or throwing a C++ style exception)");
//! [items.extern.abi.system]: "unsafe extern \"system\" — This is equivalent to extern \"C\"
//! except on Windows x86_32 where it is equivalent to \"stdcall\" for non-variadic functions",
//! and Windows is not a host blaze builds for (AGENTS.md "Not now"; the hosts are arm64 macOS
//! and Linux, x86-64 in CI), so all four map to [`Abi::C`].

use blaze_build::Abi;
use blaze_typeck as tc;

/// A fn's written ABI as the build reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct BuildAbi {
    /// The calling convention.
    pub(super) abi: Abi,
    /// Whether the ABI is of the Reference's "non-unwinding" category: a panic reaching a body
    /// of it aborts, a call of a foreign fn of it never unwinds.
    pub(super) nounwind: bool,
}

/// The ABI a fn's own head is written with (front2 heads.rs `abi_of`, read by types.rs
/// `fn_ptr_abi`): every ABI a head accepts has a reading, so a new one in the checker's
/// [`tc::Abi`] stops this match from compiling instead of being guessed.
pub(super) fn of_head(abi: tc::Abi) -> BuildAbi {
    match abi {
        tc::Abi::Rust => BuildAbi { abi: Abi::Rust, nounwind: false },
        tc::Abi::RustCall => BuildAbi { abi: Abi::RustCall, nounwind: false },
        tc::Abi::C => BuildAbi { abi: Abi::C, nounwind: true },
        tc::Abi::CUnwind => BuildAbi { abi: Abi::C, nounwind: false },
        tc::Abi::System => BuildAbi { abi: Abi::C, nounwind: true },
        tc::Abi::SystemUnwind => BuildAbi { abi: Abi::C, nounwind: false },
    }
}

/// The ABI of an `extern` block, as program.rs `placed` gives it (unquoted; a bare `extern` is
/// "C"): the ABI of every fn the block declares. `None` for any other string: the caller
/// refuses the fn by name (`Front2Refusal::Abi`).
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
    /// Expected: `test front2::program::unwind_abi::tests::c_and_c_unwind_differ_for_a_definition_and_a_declaration ... ok`
    #[test]
    fn c_and_c_unwind_differ_for_a_definition_and_a_declaration() {
        let r = Range::new(0, 0);
        let defined = |written: &str| of_head(fn_ptr_abi(written, r).ok().expect("an ABI the head accepts"));
        // A definition: `extern "C" fn f() {}` aborts at its edge; `extern "C-unwind"` unwinds.
        assert_eq!(defined("\"C\""), BuildAbi { abi: Abi::C, nounwind: true });
        assert_eq!(defined("\"C-unwind\""), BuildAbi { abi: Abi::C, nounwind: false });
        assert_eq!(defined("\"system\""), BuildAbi { abi: Abi::C, nounwind: true });
        assert_eq!(defined("\"system-unwind\""), BuildAbi { abi: Abi::C, nounwind: false });
        assert_eq!(defined("\"Rust\""), BuildAbi { abi: Abi::Rust, nounwind: false });
        assert_eq!(defined("\"rust-call\""), BuildAbi { abi: Abi::RustCall, nounwind: false });
        // A declaration: a call of a foreign "C" fn never unwinds; of a "C-unwind" one, may.
        assert_eq!(of_block("C"), Some(BuildAbi { abi: Abi::C, nounwind: true }));
        assert_eq!(of_block("C-unwind"), Some(BuildAbi { abi: Abi::C, nounwind: false }));
        assert_eq!(of_block("system"), Some(BuildAbi { abi: Abi::C, nounwind: true }));
        assert_eq!(of_block("system-unwind"), Some(BuildAbi { abi: Abi::C, nounwind: false }));
        assert_eq!(of_block("Rust"), Some(BuildAbi { abi: Abi::Rust, nounwind: false }));
        // An ABI the build has no reading of is refused by its caller, never guessed.
        assert_eq!(of_block("stdcall"), None);
        std::println!("unwind_abi: \"C\" aborts, \"C-unwind\" unwinds, for a definition and a declaration");
    }
}
