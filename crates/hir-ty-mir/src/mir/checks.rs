//! The checks an optimized body still makes: each `Assert` (a bounds check, an overflow check,
//! a division by zero, a UB check) and each call of a toolchain function that panics or may
//! (`panic_bounds_check`, `panic_const_*`, `panic_fmt`, `option::unwrap_failed`,
//! `result::unwrap_failed`, an `unwrap` or `expect` the optimizer did not inline, an
//! `Index::index` it did not inline: [`PANICKING`]), each placed at the expression of the body's own source it
//! comes from: an inlined callee's check at the call it was inlined at.
//!
//! What `#[blaze::checkfree]` holds a function to (blaze-lints' `blaze::checkfree`, the stencil
//! backend's check before it emits the function) and what `--bce-lines` counts per line.
//! It reads the body only: a call of a function the optimizer kept out of line is a check only
//! when it is one of the toolchain's panicking functions above, never followed into its body
//! (a crate function that panics inside, or a toolchain one [`PANICKING`] does not name, as
//! `Vec::remove`, is not found here; the LLVM backend's check reads the module).

use base_db::{CrateOrigin, Db, UnnamedCrate};
use hir_def::{FunctionId, HasModule as _, signatures::FunctionSignature};

use super::{AssertKind, BinOp, MirBody, MirSpan, TerminatorKind};

/// How many inlined scopes up a span is followed to the body's own source.
const SCOPES: usize = 256;

/// One check an optimized body makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Where in the body's own source it is: an inlined callee's at its call site. `Unknown`
    /// when no span of the body's source reaches it (the caller says so, never drops it).
    pub span: MirSpan,
    pub kind: CheckKind,
    /// It came from a callee the optimizer inlined.
    pub inlined: bool,
}

/// What a [`Check`] checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckKind {
    BoundsCheck,
    /// An arithmetic overflow check; `optional` when a unit built without overflow checks
    /// drops it (`AssertKind::is_optional_overflow_check`).
    Overflow {
        op: BinOp,
        optional: bool,
    },
    NegOverflow,
    DivisionByZero,
    RemainderByZero,
    /// A UB check (`check_pointers`, `check_enums`), made only where UB checks are on.
    UbCheck(&'static str),
    /// A call of the toolchain function named so (`core::panicking::panic_fmt`), which panics
    /// or may.
    Panics(String),
}

impl CheckKind {
    /// As an error names it.
    pub fn say(&self) -> String {
        match self {
            CheckKind::BoundsCheck => "a bounds check".to_owned(), // alloc: a message's part
            CheckKind::Overflow { op, .. } => format!("an overflow check of `{op:?}`"),
            CheckKind::NegOverflow => "an overflow check of a negation".to_owned(), // alloc: as above
            CheckKind::DivisionByZero => "a division-by-zero check".to_owned(), // alloc: as above
            CheckKind::RemainderByZero => "a remainder-by-zero check".to_owned(), // alloc: as above
            CheckKind::UbCheck(it) => format!("a UB check ({it})"),
            CheckKind::Panics(it) => format!("a call of `{it}`, which panics"),
        }
    }

    /// Whether a unit built without overflow checks drops it.
    pub fn is_optional(&self) -> bool {
        matches!(self, CheckKind::Overflow { optional: true, .. } | CheckKind::NegOverflow)
    }
}

/// A call of a toolchain function [`PANICKING`] names, in a crate with no name: the check is
/// said by the function's path, which starts at its crate's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnnamedPanicking {
    /// The function's path under its crate (`panicking::panic_fmt`).
    pub path: String,
    pub krate: UnnamedCrate,
}

impl std::fmt::Display for UnnamedPanicking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a call of `{}` of {}, a toolchain crate with no name: a check is said by its function's \
             path, which starts at the crate's name",
            self.path, self.krate
        )
    }
}

/// The checks `body` makes, in the order of its blocks.
pub fn checks(db: &Db, body: &MirBody<'_>) -> Result<Vec<Check>, UnnamedPanicking> {
    // alloc: the checks of one body, a handful, returned to the caller
    let mut out = Vec::new();
    for (_, block) in body.basic_blocks.iter() {
        let Some(terminator) = &block.terminator else { continue };
        let kind = match &terminator.kind {
            TerminatorKind::Assert { msg, .. } => Some(assert_kind(msg)),
            TerminatorKind::Call { func, .. } => match func.fn_item() {
                Some((f, _)) => panicking(db, f)?.map(CheckKind::Panics),
                None => None,
            },
            _ => None,
        };
        let Some(kind) = kind else { continue };
        let (span, inlined) = source_span(body, terminator.span);
        out.push(Check { span, kind, inlined });
    }
    Ok(out)
}

fn assert_kind(msg: &AssertKind) -> CheckKind {
    match msg {
        AssertKind::BoundsCheck { .. } => CheckKind::BoundsCheck,
        AssertKind::Overflow(op, ..) => {
            CheckKind::Overflow { op: op.clone(), optional: msg.is_optional_overflow_check() }
        }
        AssertKind::OverflowNeg(_) => CheckKind::NegOverflow,
        AssertKind::DivisionByZero(_) => CheckKind::DivisionByZero,
        AssertKind::RemainderByZero(_) => CheckKind::RemainderByZero,
        AssertKind::MisalignedPointerDereference { .. } => {
            CheckKind::UbCheck("a misaligned pointer dereference")
        }
        AssertKind::NullPointerDereference => CheckKind::UbCheck("a null pointer dereference"),
        AssertKind::NullReferenceConstructed => {
            CheckKind::UbCheck("a reference made from a null pointer")
        }
        AssertKind::InvalidEnumConstruction(_) => CheckKind::UbCheck("an invalid enum value"),
    }
}

/// The toolchain's functions a call of which is a check, by the module under the crate root
/// they are in, with their names; any name of the module when the list is empty. Besides
/// these, any toolchain function named `index` or `index_mut` (an `Index` impl kept out of
/// line: `Vec`'s, a slice's, a map's, which panic on a missing index or key) and any of
/// `slice`'s named `*_fail` (`slice_index_fail`, `slice_end_index_len_fail`).
const PANICKING: &[(&str, &[&str])] = &[
    ("panicking", &[]),
    ("panic", &["panic_any", "resume_unwind"]),
    ("rt", &["begin_panic", "panic_fmt", "panic_display"]),
    ("option", &["unwrap_failed", "expect_failed", "unwrap", "expect"]),
    ("result", &["unwrap_failed", "unwrap", "expect", "unwrap_err", "expect_err"]),
    ("str", &["slice_error_fail"]),
];

/// The path of `f` when it is a toolchain function [`PANICKING`] names, as an error names it.
fn panicking(db: &Db, f: FunctionId) -> Result<Option<String>, UnnamedPanicking> {
    let module = f.module(db).to_db(db);
    let krate = module.krate(db);
    if !matches!(krate.data(db).origin, CrateOrigin::Lang(_)) {
        return Ok(None);
    }
    let name = FunctionSignature::of(db, f).name.as_str();
    // The module right under the crate root that `f` is in (`panicking` of `core::panicking`).
    let mut at = module;
    let mut top = None;
    for _ in 0..SCOPES {
        let Some(parent) = at.containing_module(db) else { break };
        if parent.containing_module(db).is_none() {
            top = at.name(db);
            break;
        }
        at = parent;
    }
    let Some(top) = top else { return Ok(None) };
    let top = top.as_str();
    let listed =
        PANICKING.iter().any(|(m, names)| *m == top && (names.is_empty() || names.contains(&name)));
    let index = matches!(name, "index" | "index_mut");
    let slice_fail = top == "slice" && name.ends_with("_fail");
    if !(listed || index || slice_fail) {
        return Ok(None);
    }
    match krate.rust_name(db) {
        Ok(crate_name) => Ok(Some(format!("{crate_name}::{top}::{name}"))),
        Err(krate) => Err(UnnamedPanicking { path: format!("{top}::{name}"), krate }),
    }
}

/// `span` placed in the body's own source, and whether it came from an inlined callee: an
/// inlined callee's span at the call it was inlined at. `hir-ty-pure` places the calls and
/// operations a `#[blaze::pure]` refusal names with it.
pub fn source_span(body: &MirBody<'_>, span: MirSpan) -> (MirSpan, bool) {
    let mut at = span;
    let mut inlined = false;
    for _ in 0..SCOPES {
        let MirSpan::Inlined(n) = at else { return (at, inlined) };
        inlined = true;
        let Some(it) = body.inlined.spans.get(n as usize) else { return (MirSpan::Unknown, true) };
        let Some(scope) = body.inlined.scopes.get(it.scope as usize) else {
            return (MirSpan::Unknown, true);
        };
        at = scope.call_site;
    }
    (MirSpan::Unknown, inlined)
}

/// The checks of `body` by the line each is on: `line_of` gives the line of a span of the
/// body's source (`None` for one it can't place, counted under line 0), lines in order and each
/// line's checks in the body's block order. For `--bce-lines`.
pub fn by_line(
    checks: &[Check],
    mut line_of: impl FnMut(MirSpan) -> Option<u32>,
) -> Vec<(u32, Vec<CheckKind>)> {
    // alloc: one function's checks with their lines, sorted so the output is the same every run
    let mut placed: Vec<(u32, usize)> =
        checks.iter().enumerate().map(|(at, it)| (line_of(it.span).unwrap_or(0), at)).collect();
    placed.sort_unstable();
    // alloc: one function's lines, returned to the caller
    let mut out: Vec<(u32, Vec<CheckKind>)> = Vec::new();
    for (line, at) in placed {
        let kind = checks[at].kind.clone(); // clone: one row per check of the report
        match out.last_mut() {
            Some((last, kinds)) if *last == line => kinds.push(kind),
            // alloc: one line's checks, a few
            _ => out.push((line, vec![kind])),
        }
    }
    out
}
