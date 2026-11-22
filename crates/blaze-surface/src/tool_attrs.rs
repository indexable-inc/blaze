//! Tool attributes: the namespaces whose attributes are inert ([`TOOLS`]), and the one table of
//! the `blaze::` attributes this build knows ([`KNOWN`]), each with the stage that reads it.
//!
//! Rust's rules, which this follows (the Reference, "Attributes", "Tool attributes"): an
//! attribute whose path starts with a tool's name (`rustfmt`, `clippy`, `diagnostic`, a
//! `#![register_tool(..)]`'s) is inert, is never looked up as a macro, and the compiler checks
//! none of it: an unknown `#[clippy::x]` is not an error. A plain attribute nobody defines is
//! (`cannot find attribute `x` in this scope`), and so is a path attribute whose first segment
//! is neither a tool nor a crate or module (`#[foo::bar]`: the path does not resolve).
//!
//! blaze is a tool that is always registered, as `rustfmt` and `clippy` are: `#[blaze::x]` is
//! inert, kept on its item's record as written (the lowered form's attribute rows: an
//! item-relative path range and token range), never a macro and never a refusal. Unlike
//! rustc for its tools, blaze knows its own namespace: a name that is not in [`KNOWN`] is one
//! advice-level finding naming it, with the nearest known name ([`classify`]). A known name is
//! in one [`State`]: unchecked (recorded, and the commands say so with one counted note, never
//! as if it were checked), checked by a stage, or removed (an error whose fix deletes it).

/// The tool namespaces, sorted: an attribute path of two or more segments whose first segment
/// is one of these is inert.
pub const TOOLS: [&str; 7] = ["blaze", "clippy", "diagnostic", "miri", "rust_analyzer", "rustdoc", "rustfmt"];

/// blaze's own tool namespace.
pub const BLAZE: &str = "blaze";

/// A stage of blaze that reads attributes: the lanes of the extensions ledger
/// (docs/v2/plan/design/extensions.md, section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// The effect summary and its row (`noalloc`, `pure`, `sans_io`, ..).
    Effects,
    /// Linear values, early drop, handles.
    Ownership,
    /// Comptime and reflection: marks, `memo`, `cache`.
    Comptime,
    /// A crate's syntax bits, read by the outline and the checker.
    Syntax,
    /// The solver's facts on traits and impls: `sealed`, specialization.
    Traits,
    /// Theorems, contracts, invariants.
    Proofs,
    /// Code generation: multiversioning, SIMD, autodiff.
    Codegen,
}

/// What this build does with a known `blaze::` attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Known, unchecked: recorded on its item, read by no stage. The commands count it in their
    /// one note, so it never reads as checked.
    Unchecked,
    /// Checked by this stage.
    Checked(Stage),
    /// Removed from the language: each use is an error whose fix deletes it; `why` says what
    /// enforces it now (a bstd capability in the signature, ..).
    Removed { why: &'static str },
}

/// One `blaze::` attribute this build knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownAttr {
    /// The name after `blaze::`.
    pub name: &'static str,
    pub state: State,
    /// The stage meant to read it (the ledger's lane).
    pub planned: Stage,
}

const fn known(name: &'static str, planned: Stage) -> KnownAttr {
    KnownAttr { name, state: State::Unchecked, planned }
}

/// Every `blaze::` attribute this build knows, sorted by name: the extensions ledger's section 3
/// (2026-11-16), every one known and unchecked today but `args` (rule 83, 2026-11-17). A stage that starts to check one, or the
/// verdict that removes one, changes its `state` here and nowhere else.
pub const KNOWN: [KnownAttr; 52] = {
    use Stage::*;
    [
        known("answers_request", Effects),
        known("arbitrary_ints", Syntax),
        // An options struct (AGENTS.md rule 83, D126): checked by the body check's `args-inline`
        // (blaze-typeck rules/args_inline.rs), read from the ADT's head (front2 heads.rs).
        KnownAttr { name: "args", state: State::Checked(Syntax), planned: Syntax },
        known("attr", Comptime),
        known("bits", Syntax),
        known("bounded_turn", Effects),
        known("cache", Comptime),
        known("canonical_cbor", Comptime),
        known("capabilities", Effects),
        known("checkfree", Proofs),
        known("consumes", Ownership),
        known("contract_test", Proofs),
        known("differentiable", Codegen),
        // `#![blaze::driver]`: the thin driver over a sans-IO core, which holds the handles and
        // does the IO the core asks for (D170 C5; blaze-lint `RootMarks`, `sans-io-core`).
        known("driver", Effects),
        known("drop", Ownership),
        known("early_drop", Ownership),
        known("edition", Syntax),
        known("effects", Effects),
        known("error_unions", Syntax),
        known("expect", Effects),
        known("handles", Ownership),
        known("holds", Ownership),
        known("invariant", Proofs),
        known("linear", Ownership),
        known("lock_checked", Effects),
        // `#[blaze::lock_exception(tried = "published"|"gathered"|"claimed", why = "..")]` on
        // the statement, field, static, const or item that constructs a lock: which D179 shape
        // it was tried as and why that failed (blaze-lint `lock-without-reason` reads it).
        known("lock_exception", Effects),
        known("mark", Comptime),
        known("may", Effects),
        known("may_suspend", Effects),
        known("memo", Comptime),
        known("multiversion", Codegen),
        known("noalloc", Effects),
        known("nonblocking", Effects),
        known("nopanic", Effects),
        known("norefcount", Ownership),
        known("pairs_with", Effects),
        known("pattern_types", Syntax),
        known("policy", Effects),
        known("program_lifetime", Effects),
        known("pure", Effects),
        known("records", Syntax),
        known("reflect", Comptime),
        known("renders_positions", Effects),
        known("sans_io", Effects),
        known("sealed", Traits),
        known("simd", Codegen),
        known("specialization", Traits),
        known("specialization_trait", Traits),
        known("theorem", Proofs),
        known("trust", Effects),
        known("uniq", Syntax),
        known("views", Syntax),
    ]
};

/// What the name after `blaze::` is to this build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlazeAttr {
    /// `KNOWN[index]`.
    Known(u16),
    /// No attribute this build knows; `nearest`: the index in [`KNOWN`] of the known name within
    /// edit distance 2 of it, the closest (the first in the table on a tie), when there is one.
    Unknown { nearest: Option<u16> },
}

/// The largest edit distance [`classify`] suggests a known name at.
pub const NEAREST_DISTANCE: usize = 2;

/// Reads `name`, the segment after `blaze::` (one segment: `blaze::a::b` passes `a::b`, which no
/// known name equals).
pub fn classify(name: &str) -> BlazeAttr {
    if let Ok(i) = KNOWN.binary_search_by(|k| k.name.cmp(name)) {
        return BlazeAttr::Known(i as u16);
    }
    let mut nearest: Option<(usize, u16)> = None;
    for (i, k) in KNOWN.iter().enumerate() {
        if let Some(d) = distance_within(name.as_bytes(), k.name.as_bytes(), NEAREST_DISTANCE)
            && nearest.is_none_or(|(best, _)| d < best)
        {
            nearest = Some((d, i as u16));
        }
    }
    BlazeAttr::Unknown { nearest: nearest.map(|(_, i)| i) }
}

/// The longest name [`distance_within`] compares; a longer one has no nearest name.
const LONGEST: usize = 40;

/// The Levenshtein distance of `a` and `b` (bytes) when it is at most `bound`; `None` past it,
/// or when either is longer than [`LONGEST`]. Two rows on the stack, no allocation.
fn distance_within(a: &[u8], b: &[u8], bound: usize) -> Option<usize> {
    if a.len() > LONGEST || b.len() > LONGEST || a.len().abs_diff(b.len()) > bound {
        return None;
    }
    let mut prev = [0usize; LONGEST + 1];
    let mut cur = [0usize; LONGEST + 1];
    for (j, slot) in prev.iter_mut().enumerate().take(b.len() + 1) {
        *slot = j;
    }
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let swap = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + swap).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[b.len()];
    (d <= bound).then_some(d)
}
