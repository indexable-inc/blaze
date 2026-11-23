//! Pure `#![no_std]` text & slice diffing and unified patch creation, parsing, and application.
//!
//! Covers every granularity of a modern batteries-included diff engine:
//!
//! - **Slices & arrays**: [`slices`] / [`arrays`] (`&[T]` for any `T: Eq`, zero-copy [`Change`]).
//! - **Text granularities**: [`lines`], [`words`], [`chars`], and [`bytes`] (coalesced zero-copy
//!   [`TextChange`] / [`Change`] runs whose `Equal + Delete` reconstructs `old` byte-for-byte and
//!   `Equal + Insert` reconstructs `new` byte-for-byte).
//! - **Unified diff hunks & patches**: [`hunks`], [`create_patch`], [`write_patch`],
//!   [`format_patch_ansi`], [`parse_patch`], [`parse_patches`], [`apply_patch`],
//!   [`apply_patch_text`], and [`apply_patch_reverse`].
//! - **Metrics**: exact Myers [`edit_distance`] and normalized [`similarity`].
//!
//! # Algorithms & bounds
//!
//! - [`Algorithm::Myers`]: iterative linear-space divide-and-conquer Myers O(ND) (Myers 1986, §4b)
//!   using `O(N + M)` scratch space, common prefix/suffix stripping, and zero recursion.
//! - [`Algorithm::Patience`]: iterative unique-element LIS anchoring (Patience diff) with Myers on
//!   gaps — cleaner human-readable diffs for source code and structured formats.
//! - Every diff call takes an explicit step budget (`DiffOpts::budget`) and allocator (`alloc`),
//!   and every patch parser takes explicit [`ParsePatchLimits`]: adversarial inputs end in a typed
//!   error rather than unbounded work or memory.

use core::alloc::Allocator;
use core::fmt;
use core::hash::Hash;

use blaze_own::{AllocFailure, String, Vec};

mod merge;
mod myers;
mod patch;
mod patience;
#[cfg(test)]
mod tests;
mod tokenize;
mod value;

pub use merge::{ConflictStyle, MergeOpts, MergeRegion, MergeResult, merge3};
pub use myers::DEFAULT_BUDGET;
pub use patch::{DiffStats, Hunk, HunkLine, Patch};
use tokenize::Token;
pub use value::{ValuePatchError, ValuePatchOp, apply_value_patch, diff_values};

/// Which diff algorithm to run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Algorithm {
    /// Linear-space Myers O(ND) shortest edit script.
    Myers,
    /// Patience unique-element LIS anchoring with Myers on gaps.
    Patience,
}

/// Low-level edit operation over slice indices.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// `old[old_start..old_start + len] == new[new_start..new_start + len]`.
    Equal { old_start: usize, new_start: usize, len: usize },
    /// `old[old_start..old_start + len]` was removed.
    Delete { old_start: usize, len: usize },
    /// `new[new_start..new_start + len]` was added.
    Insert { new_start: usize, len: usize },
}

/// A coalesced run of unchanged, removed, or added elements borrowing directly from `old` or `new`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change<'a, T> {
    Equal(&'a [T]),
    Delete(&'a [T]),
    Insert(&'a [T]),
}

/// A coalesced run of unchanged, removed, or added text borrowing directly from `old` or `new`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextChange<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

/// Options for [`slices`], [`arrays`], [`lines`], [`words`], [`chars`], and [`bytes`].
#[derive(Clone, Copy, Debug)]
pub struct DiffOpts<A: Allocator> {
    /// Algorithm choice (`Myers` or `Patience`).
    pub algorithm: Algorithm,
    /// Maximum search steps before returning [`DiffError::BudgetExceeded`].
    pub budget: usize,
    /// Allocator for scratch buffers and the returned edit list.
    pub alloc: A,
}

/// Options for [`hunks`], [`create_patch`], and [`write_patch`].
#[derive(Clone, Copy, Debug)]
pub struct PatchOpts<'a, A: Allocator> {
    /// Algorithm choice (`Myers` or `Patience`).
    pub algorithm: Algorithm,
    /// Number of unchanged context lines around each change (typically `3`).
    pub context: usize,
    /// Optional `--- <old_name>` file header (emitted only when both names are `Some`).
    pub old_name: Option<&'a str>,
    /// Optional `+++ <new_name>` file header (emitted only when both names are `Some`).
    pub new_name: Option<&'a str>,
    /// Maximum search steps before returning [`DiffError::BudgetExceeded`].
    pub budget: usize,
    /// Allocator for scratch buffers and the output.
    pub alloc: A,
}

/// Bounds enforced while parsing untrusted unified diff text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParsePatchLimits {
    /// Maximum patch byte length.
    pub most_bytes: usize,
    /// Maximum total hunks across all files in the patch.
    pub most_hunks: usize,
    /// Maximum total hunk body lines across all hunks.
    pub most_lines: usize,
}

impl ParsePatchLimits {
    /// Generous default limits suitable for source patches (16 MiB, 16,384 hunks, 262,144 lines).
    pub const USUAL: Self = Self {
        most_bytes: 16 << 20,
        most_hunks: 16_384,
        most_lines: 262_144,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED` / `blaze_toml::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Options for [`apply_patch`], [`apply_patch_text`], and [`apply_patch_reverse`].
#[derive(Clone, Copy, Debug)]
pub struct ApplyOpts<A: Allocator> {
    /// Maximum line offset from a hunk's stated position to search when applying (`0` = strict).
    pub max_offset: usize,
    /// Allocator for the resulting text and scratch line index.
    pub alloc: A,
}

/// Why computing a diff failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffError {
    /// Search reached its explicit step budget (`bounded-progress`).
    BudgetExceeded { steps: usize },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DiffError::BudgetExceeded { steps } => {
                write!(f, "bstd::diff: edit search exceeded its step budget ({steps} steps)")
            }
            DiffError::Alloc(err) => write!(f, "bstd::diff: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for DiffError {}

/// Why parsing a unified diff patch failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParsePatchError {
    /// 1-based line number in the patch text where parsing stopped.
    pub line: usize,
    /// Specific syntactic or bound violation.
    pub why: ParseWhy,
}

/// Specific reason [`parse_patch`] or [`parse_patches`] refused input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseWhy {
    InputTooLarge { bytes: usize, limit: usize },
    TooManyHunks { limit: usize },
    TooManyLines { limit: usize },
    MissingNewHeader,
    InvalidHunkHeader,
    InvalidLinePrefix { byte: u8 },
    UnexpectedNoNewlineMarker,
    HunkLineCountMismatch {
        expected_old: usize,
        actual_old: usize,
        expected_new: usize,
        actual_new: usize,
    },
    UnexpectedLine,
    MultipleFiles,
    OutOfMemory,
}

impl fmt::Display for ParsePatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::diff: patch line {}: ", self.line)?;
        match self.why {
            ParseWhy::InputTooLarge { bytes, limit } => {
                write!(f, "patch size {bytes} bytes exceeds limit {limit}")
            }
            ParseWhy::TooManyHunks { limit } => write!(f, "hunk count exceeds limit {limit}"),
            ParseWhy::TooManyLines { limit } => write!(f, "hunk line count exceeds limit {limit}"),
            ParseWhy::MissingNewHeader => {
                f.write_str("expected `+++ <new>` header after `--- <old>`")
            }
            ParseWhy::InvalidHunkHeader => f.write_str("malformed `@@ -a,b +c,d @@` hunk header"),
            ParseWhy::InvalidLinePrefix { byte } => {
                write!(f, "unexpected hunk line prefix byte 0x{byte:02x}")
            }
            ParseWhy::UnexpectedNoNewlineMarker => {
                f.write_str("`\\ No newline at end of file` without a preceding hunk line")
            }
            ParseWhy::HunkLineCountMismatch {
                expected_old,
                actual_old,
                expected_new,
                actual_new,
            } => write!(
                f,
                "hunk line counts do not match header (expected -{expected_old}/+{expected_new}, got -{actual_old}/+{actual_new})"
            ),
            ParseWhy::UnexpectedLine => f.write_str("unexpected line outside hunk"),
            ParseWhy::MultipleFiles => {
                f.write_str("patch contains multiple files; use `parse_patches`")
            }
            ParseWhy::OutOfMemory => f.write_str("allocator refused allocation"),
        }
    }
}

impl core::error::Error for ParsePatchError {}

/// Why applying a patch to a base text failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ApplyPatchError {
    /// A hunk's context or deleted lines did not match the base text.
    HunkMismatch { hunk: usize, expected_line: usize },
    /// A hunk with non-zero line count stated a 0 start line.
    InvalidHunkStart { hunk: usize, start: usize },
    /// Parsing raw patch text failed (for [`apply_patch_text`]).
    Parse(ParsePatchError),
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for ApplyPatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ApplyPatchError::HunkMismatch { hunk, expected_line } => {
                write!(
                    f,
                    "bstd::diff: hunk #{} failed to apply at line {expected_line}",
                    hunk + 1
                )
            }
            ApplyPatchError::InvalidHunkStart { hunk, start } => {
                write!(f, "bstd::diff: hunk #{} has invalid start line {start}", hunk + 1)
            }
            ApplyPatchError::Parse(err) => write!(f, "{err}"),
            ApplyPatchError::Alloc(err) => write!(f, "bstd::diff: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for ApplyPatchError {}

/// Test-only planted bugs for verifying our property-test oracles (`testing` skill).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DiffPlant {
    None,
    /// Omit `\ No newline at end of file` markers when building hunks.
    SkipNoNewlineMarker,
    /// Corrupt the old line count in formatted hunk headers by +1.
    OffByOneHunkCount,
}

/// Compute low-level edit [`Op`]s transforming `old` into `new` with Myers (works for any `T: Eq`).
pub fn ops_myers<T: Eq, A: Allocator + Copy>(
    old: &[T],
    new: &[T],
    budget: usize,
    alloc: A,
) -> Result<Vec<Op, A>, DiffError> {
    myers::diff_myers(old, new, budget, alloc)
}

/// Compute low-level edit [`Op`]s transforming `old` into `new` with either Myers or Patience.
pub fn ops<T: Eq + Hash, A: Allocator + Copy>(
    old: &[T],
    new: &[T],
    opts: DiffOpts<A>,
) -> Result<Vec<Op, A>, DiffError> {
    match opts.algorithm {
        Algorithm::Myers => myers::diff_myers(old, new, opts.budget, opts.alloc),
        Algorithm::Patience => patience::diff_patience(old, new, opts.budget, opts.alloc),
    }
}

/// Diff two arbitrary slices (`&[T]` where `T: Eq`) using Myers O(ND), returning coalesced
/// zero-copy [`Change`] slices.
pub fn slices<'a, T: Eq, A: Allocator + Copy>(
    old: &'a [T],
    new: &'a [T],
    budget: usize,
    alloc: A,
) -> Result<Vec<Change<'a, T>, A>, DiffError> {
    let edit_ops = myers::diff_myers(old, new, budget, alloc)?;
    ops_to_changes(old, new, edit_ops.as_slice(), alloc)
}

/// Alias for [`slices`] (matching `Bun.diff.arrays`) supporting both Myers and Patience for
/// hashable elements (`T: Eq + Hash`).
pub fn arrays<'a, T: Eq + Hash, A: Allocator + Copy>(
    old: &'a [T],
    new: &'a [T],
    opts: DiffOpts<A>,
) -> Result<Vec<Change<'a, T>, A>, DiffError> {
    let edit_ops = ops(old, new, opts)?;
    ops_to_changes(old, new, edit_ops.as_slice(), opts.alloc)
}

/// Diff two byte slices (`&[u8]`), returning coalesced zero-copy [`Change`] slices.
pub fn bytes<'a, A: Allocator + Copy>(
    old: &'a [u8],
    new: &'a [u8],
    opts: DiffOpts<A>,
) -> Result<Vec<Change<'a, u8>, A>, DiffError> {
    arrays(old, new, opts)
}

/// Diff two texts line-by-line, returning coalesced zero-copy [`TextChange`] runs.
pub fn lines<'a, A: Allocator + Copy>(
    old: &'a str,
    new: &'a str,
    opts: DiffOpts<A>,
) -> Result<Vec<TextChange<'a>, A>, DiffError> {
    let old_tokens = tokenize::split_line_tokens(old, opts.alloc)?;
    let new_tokens = tokenize::split_line_tokens(new, opts.alloc)?;
    let edit_ops = ops(old_tokens.as_slice(), new_tokens.as_slice(), opts)?;
    coalesce_token_ops(
        old,
        new,
        old_tokens.as_slice(),
        new_tokens.as_slice(),
        edit_ops.as_slice(),
        opts.alloc,
    )
}

/// Diff two texts word-by-word (words, whitespace runs, and punctuation), returning coalesced
/// zero-copy [`TextChange`] runs.
pub fn words<'a, A: Allocator + Copy>(
    old: &'a str,
    new: &'a str,
    opts: DiffOpts<A>,
) -> Result<Vec<TextChange<'a>, A>, DiffError> {
    let old_tokens = tokenize::split_word_tokens(old, opts.alloc)?;
    let new_tokens = tokenize::split_word_tokens(new, opts.alloc)?;
    let edit_ops = ops(old_tokens.as_slice(), new_tokens.as_slice(), opts)?;
    coalesce_token_ops(
        old,
        new,
        old_tokens.as_slice(),
        new_tokens.as_slice(),
        edit_ops.as_slice(),
        opts.alloc,
    )
}

/// Diff two texts character-by-character (`char`), returning coalesced zero-copy [`TextChange`] runs.
pub fn chars<'a, A: Allocator + Copy>(
    old: &'a str,
    new: &'a str,
    opts: DiffOpts<A>,
) -> Result<Vec<TextChange<'a>, A>, DiffError> {
    let old_tokens = tokenize::split_char_tokens(old, opts.alloc)?;
    let new_tokens = tokenize::split_char_tokens(new, opts.alloc)?;
    let edit_ops = ops(old_tokens.as_slice(), new_tokens.as_slice(), opts)?;
    coalesce_token_ops(
        old,
        new,
        old_tokens.as_slice(),
        new_tokens.as_slice(),
        edit_ops.as_slice(),
        opts.alloc,
    )
}

/// Compute unified diff [`Hunk`]s between `old` and `new`.
pub fn hunks<'a, A: Allocator + Copy>(
    old: &'a str,
    new: &'a str,
    opts: PatchOpts<'_, A>,
) -> Result<Vec<Hunk<'a, A>, A>, DiffError> {
    hunks_planted(old, new, opts, DiffPlant::None)
}

pub(crate) fn hunks_planted<'a, A: Allocator + Copy>(
    old: &'a str,
    new: &'a str,
    opts: PatchOpts<'_, A>,
    plant: DiffPlant,
) -> Result<Vec<Hunk<'a, A>, A>, DiffError> {
    let old_lines = tokenize::split_lines(old, opts.alloc)?;
    let new_lines = tokenize::split_lines(new, opts.alloc)?;
    let edit_ops = ops(
        old_lines.as_slice(),
        new_lines.as_slice(),
        DiffOpts { algorithm: opts.algorithm, budget: opts.budget, alloc: opts.alloc },
    )?;
    patch::build_hunks(
        old_lines.as_slice(),
        new_lines.as_slice(),
        edit_ops.as_slice(),
        opts.context,
        opts.alloc,
        plant,
    )
}

/// Create a standard unified diff patch transforming `old` into `new`.
///
/// Returns an empty string when `old == new`.
pub fn create_patch<A: Allocator + Copy>(
    old: &str,
    new: &str,
    opts: PatchOpts<'_, A>,
) -> Result<String<A>, DiffError> {
    create_patch_planted(old, new, opts, DiffPlant::None)
}

pub(crate) fn create_patch_planted<A: Allocator + Copy>(
    old: &str,
    new: &str,
    opts: PatchOpts<'_, A>,
    plant: DiffPlant,
) -> Result<String<A>, DiffError> {
    let alloc = opts.alloc;
    let old_name = opts.old_name;
    let new_name = opts.new_name;
    let built = hunks_planted(old, new, opts, plant)?;
    let mut out = String::new_in(alloc);
    if patch::write_patch_hunks(old_name, new_name, built.as_slice(), &mut out, plant).is_err() {
        return Err(DiffError::Alloc(AllocFailure::CapacityOverflow {
            what: "bstd::diff::create_patch",
            len: out.len(),
            additional: 1,
        }));
    }
    Ok(out)
}

/// Write a unified diff patch transforming `old` into `new` directly into any [`fmt::Write`].
pub fn write_patch<A: Allocator + Copy, W: fmt::Write>(
    old: &str,
    new: &str,
    opts: PatchOpts<'_, A>,
    out: &mut W,
) -> Result<(), DiffError> {
    let old_name = opts.old_name;
    let new_name = opts.new_name;
    let built = hunks(old, new, opts)?;
    let _ = patch::write_patch_hunks(old_name, new_name, built.as_slice(), out, DiffPlant::None);
    Ok(())
}

/// Highlight unified diff text with ANSI colors (cyan hunks, red deletions, green insertions).
pub fn format_patch_ansi<A: Allocator>(patch_text: &str, alloc: A) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(patch_text.len(), alloc)?;
    format_patch_ansi_into(patch_text, &mut out).map_err(|_| AllocFailure::CapacityOverflow {
        what: "bstd::diff::format_patch_ansi",
        len: out.len(),
        additional: 1,
    })?;
    Ok(out)
}

/// Write ANSI-colored unified diff text into `out`.
pub fn format_patch_ansi_into<W: fmt::Write>(patch_text: &str, out: &mut W) -> fmt::Result {
    patch::format_patch_ansi_into(patch_text, out)
}

/// Highlight unified diff text with ANSI colors AND inline word-level emphasis on paired `-`/`+`
/// lines (like `delta` / GitHub's inline diff view).
pub fn format_patch_ansi_inline<A: Allocator + Copy>(
    patch_text: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(patch_text.len(), alloc)?;
    format_patch_ansi_inline_into(patch_text, alloc, &mut out).map_err(|_| {
        AllocFailure::CapacityOverflow {
            what: "bstd::diff::format_patch_ansi_inline",
            len: out.len(),
            additional: 1,
        }
    })?;
    Ok(out)
}

/// Write inline word-highlighted ANSI unified diff text into `out`.
pub fn format_patch_ansi_inline_into<A: Allocator + Copy, W: fmt::Write>(
    patch_text: &str,
    alloc: A,
    out: &mut W,
) -> fmt::Result {
    patch::format_patch_ansi_inline_into(patch_text, alloc, out)
}

/// Parse a single-file unified diff patch under explicit [`ParsePatchLimits`].
pub fn parse_patch<'a, A: Allocator + Copy>(
    patch_text: &'a str,
    limits: ParsePatchLimits,
    alloc: A,
) -> Result<Patch<'a, A>, ParsePatchError> {
    patch::parse_single_patch(patch_text, limits, alloc)
}

/// Parse a multi-file unified diff patch under explicit [`ParsePatchLimits`].
pub fn parse_patches<'a, A: Allocator + Copy>(
    patch_text: &'a str,
    limits: ParsePatchLimits,
    alloc: A,
) -> Result<Vec<Patch<'a, A>, A>, ParsePatchError> {
    patch::parse_multi_patch(patch_text, limits, alloc)
}

/// Apply a parsed [`Patch`] to `old` text.
pub fn apply_patch<A: Allocator + Copy, B: Allocator>(
    old: &str,
    parsed: &Patch<'_, B>,
    opts: ApplyOpts<A>,
) -> Result<String<A>, ApplyPatchError> {
    patch::apply_hunks(old, parsed.hunks.as_slice(), opts.max_offset, false, opts.alloc)
}

/// Parse unified diff `patch_text` and apply it to `old` text in one step.
pub fn apply_patch_text<A: Allocator + Copy>(
    old: &str,
    patch_text: &str,
    limits: ParsePatchLimits,
    opts: ApplyOpts<A>,
) -> Result<String<A>, ApplyPatchError> {
    let parsed = parse_patch(patch_text, limits, opts.alloc).map_err(ApplyPatchError::Parse)?;
    apply_patch(old, &parsed, opts)
}

/// Apply a parsed [`Patch`] in reverse to `new` text, recovering `old`.
pub fn apply_patch_reverse<A: Allocator + Copy, B: Allocator>(
    new: &str,
    parsed: &Patch<'_, B>,
    opts: ApplyOpts<A>,
) -> Result<String<A>, ApplyPatchError> {
    patch::apply_hunks(new, parsed.hunks.as_slice(), opts.max_offset, true, opts.alloc)
}

/// Exact Myers edit distance (minimum insertions + deletions) between `old` and `new`.
pub fn edit_distance<T: Eq, A: Allocator + Copy>(
    old: &[T],
    new: &[T],
    budget: usize,
    alloc: A,
) -> Result<usize, DiffError> {
    myers::edit_distance_myers(old, new, budget, alloc)
}

/// Normalized similarity ratio in `0.0..=1.0` (`2 * lcs / (old.len() + new.len())`, `1.0` when
/// both are empty).
pub fn similarity<T: Eq, A: Allocator + Copy>(
    old: &[T],
    new: &[T],
    budget: usize,
    alloc: A,
) -> Result<f64, DiffError> {
    let total = old.len().saturating_add(new.len());
    if total == 0 {
        return Ok(1.0);
    }
    let dist = edit_distance(old, new, budget, alloc)?;
    let matched = total.saturating_sub(dist);
    // Exact integer ratio scaled to 1_000_000 (parts per million) so no `as` cast is needed.
    let ppm = u128::try_from(matched)
        .unwrap_or(0)
        .saturating_mul(1_000_000)
        / u128::try_from(total).unwrap_or(1);
    let ppm_u32 = u32::try_from(ppm).unwrap_or(1_000_000);
    Ok(f64::from(ppm_u32) / 1_000_000.0)
}

fn ops_to_changes<'a, T, A: Allocator>(
    old: &'a [T],
    new: &'a [T],
    ops: &[Op],
    alloc: A,
) -> Result<Vec<Change<'a, T>, A>, DiffError> {
    let mut out = Vec::new_in(alloc);
    out.try_reserve(ops.len()).map_err(DiffError::Alloc)?;
    for op in ops {
        let ch = match *op {
            Op::Equal { old_start, len, .. } => {
                Change::Equal(old.get(old_start..(old_start + len)).unwrap_or(&[]))
            }
            Op::Delete { old_start, len } => {
                Change::Delete(old.get(old_start..(old_start + len)).unwrap_or(&[]))
            }
            Op::Insert { new_start, len } => {
                Change::Insert(new.get(new_start..(new_start + len)).unwrap_or(&[]))
            }
        };
        out.try_push(ch).map_err(|r| DiffError::Alloc(r.why))?;
    }
    Ok(out)
}

/// Convert token-level [`Op`]s back into contiguous borrowed `&'a str` slices using each
/// [`Token`]'s recorded byte span — zero allocations, zero casts, zero unsafe.
fn coalesce_token_ops<'a, A: Allocator>(
    old: &'a str,
    new: &'a str,
    old_tokens: &[Token<'a>],
    new_tokens: &[Token<'a>],
    ops: &[Op],
    alloc: A,
) -> Result<Vec<TextChange<'a>, A>, DiffError> {
    let mut out = Vec::new_in(alloc);
    out.try_reserve(ops.len()).map_err(DiffError::Alloc)?;

    for op in ops {
        let ch = match *op {
            Op::Equal { old_start, len, .. } => {
                let slice = span_of_tokens(old, old_tokens.get(old_start..(old_start + len)));
                TextChange::Equal(slice)
            }
            Op::Delete { old_start, len } => {
                let slice = span_of_tokens(old, old_tokens.get(old_start..(old_start + len)));
                TextChange::Delete(slice)
            }
            Op::Insert { new_start, len } => {
                let slice = span_of_tokens(new, new_tokens.get(new_start..(new_start + len)));
                TextChange::Insert(slice)
            }
        };
        out.try_push(ch).map_err(|r| DiffError::Alloc(r.why))?;
    }
    Ok(out)
}

fn span_of_tokens<'a>(whole: &'a str, tokens: Option<&[Token<'a>]>) -> &'a str {
    let Some(tokens) = tokens else { return "" };
    let (Some(first), Some(last)) = (tokens.first(), tokens.last()) else {
        return "";
    };
    whole.get(first.start..last.end).unwrap_or("")
}
