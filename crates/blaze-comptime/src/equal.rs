//! THE EQUALITY HARNESS's core: one compile-time call run on both engines (the wasm path of
//! today and the native path of this crate) is compared field by field, and the FIRST
//! difference comes back as a typed value, so a run over tempo's calls reports where the two
//! part, not that they do (comptime-native.md, "The equality harness").
//!
//! The order of comparison is the order of consequence: the tokens a macro returned (or the
//! script's output), then its diagnostics, then the read list, then the facts. Each side is
//! given in one canonical form: tokens and diagnostics in the bridge's wire as the token server
//! renders them (blaze-proc-macro wire.rs), the read list as checked entries (blaze-roots,
//! sorted), the facts sorted ([`crate::Facts`]). The wasm side's read list (blaze-proc-macro
//! table.rs rows `file:`, `dir:`, `stat:`, `env:`) is turned into this form by an adapter in
//! blaze-proc-macro, outside this crate: a `stat:` row is a [`blaze_roots::Seen::File`] or
//! [`blaze_roots::Seen::Listing`] here (host.rs keys a stat by content).

#![blaze::sans_io]

use blaze_roots::Entry;

use crate::host::Fact;

/// One engine's result of one call, in canonical form.
#[derive(Clone, Copy, Debug)]
pub struct Outcome<'r> {
    pub tokens: &'r [u8],
    pub diagnostics: &'r [&'r [u8]],
    pub reads: &'r [Entry<'r>],
    pub facts: &'r [Fact<'r>],
}

/// Where two results first part. `None` on a side: that side ended there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Difference<'r> {
    /// The first byte at which the token streams differ (bridge wire), with the byte on each
    /// side.
    Tokens { at: usize, wasm: Option<u8>, native: Option<u8> },
    Diagnostic { at: usize, wasm: Option<&'r [u8]>, native: Option<&'r [u8]> },
    Read { at: usize, wasm: Option<Entry<'r>>, native: Option<Entry<'r>> },
    Fact { at: usize, wasm: Option<Fact<'r>>, native: Option<Fact<'r>> },
}

fn first<T: Copy + PartialEq>(wasm: &[T], native: &[T]) -> Option<(usize, Option<T>, Option<T>)> {
    let len = wasm.len().max(native.len());
    (0..len).find_map(|at| {
        let (w, n) = (wasm.get(at).copied(), native.get(at).copied());
        (w != n).then_some((at, w, n))
    })
}

/// The first difference between the wasm engine's result and the native one's, or `None` when
/// they are equal in every field.
pub fn first_difference<'r>(wasm: &Outcome<'r>, native: &Outcome<'r>) -> Option<Difference<'r>> {
    if let Some((at, wasm, native)) = first(wasm.tokens, native.tokens) {
        return Some(Difference::Tokens { at, wasm, native });
    }
    if let Some((at, wasm, native)) = first(wasm.diagnostics, native.diagnostics) {
        return Some(Difference::Diagnostic { at, wasm, native });
    }
    if let Some((at, wasm, native)) = first(wasm.reads, native.reads) {
        return Some(Difference::Read { at, wasm, native });
    }
    first(wasm.facts, native.facts).map(|(at, wasm, native)| Difference::Fact { at, wasm, native })
}
