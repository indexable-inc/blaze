//! Why an evaluation over the body stopped: the constant, the row it stopped at (body item,
//! statement, item-relative span: rule 71) and what refused it (rule 19). The refusals are
//! blaze-consteval's ([`Refusal`]): one vocabulary for both evaluators until the old one goes.

use core::fmt;

use blaze_body::{AssertKind, Span, Stmt};
use blaze_consteval::{At, ConstError, Refusal};
use blaze_typeck::{ItemKey, Range};

/// A row of a body: the item whose body it is, the statement, and the statement's span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub body: ItemKey,
    pub stmt: Stmt,
    pub span: Span,
}

/// What stopped the evaluation.
#[derive(Debug)]
pub enum Fault<'a> {
    /// The machine refused: memory, arithmetic, a call, a layout, a bound (`StepLimit`,
    /// `CallDepth`, `MemoryLimit`), `Unreachable` for an `unreachable` row (undefined
    /// behaviour reached), `Panic` for a panic entry called.
    Refused(Refusal<'a>),
    /// An `assert` row's condition was not the value it expects: the panic its kind names
    /// (an error in a constant context: the Reference, "Constant evaluation"), as the
    /// refusal the old evaluator gives the same operation.
    Assert { kind: AssertKind, refusal: Refusal<'a> },
    /// A `terminate` row: the program would abort.
    Abort,
}

/// A refusal of one constant evaluated over the body.
#[derive(Debug)]
pub struct EvalError<'a> {
    /// The constant evaluated (its root body's item).
    pub constant: ItemKey,
    /// The row it stopped at; `None` before the first row ran.
    pub at: Option<Row>,
    pub fault: Fault<'a>,
}

impl<'a> EvalError<'a> {
    /// The same refusal as the old evaluator's error (its `At` is the row's span), for a
    /// caller that still reads [`ConstError`]: the switch keeps every caller's type.
    pub fn into_const_error(self) -> ConstError<'a> {
        let at = self.at.map(|r| At { body: r.body, range: Range { start: r.span.lo, end: r.span.hi } });
        let refusal = match self.fault {
            Fault::Refused(r) | Fault::Assert { refusal: r, .. } => r,
            Fault::Abort => Refusal::Unsupported { what: "an abort (`terminate`) in a constant" },
        };
        ConstError { constant: self.constant, at, refusal }
    }
}

impl fmt::Display for EvalError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "constant {:#x}", self.constant.0)?;
        if let Some(r) = self.at {
            write!(f, " (body {:#x}, row {}, bytes {}..{})", r.body.0, r.stmt.0, r.span.lo, r.span.hi)?;
        }
        write!(f, ": ")?;
        match &self.fault {
            Fault::Refused(r) => write!(f, "{}", RefusalText(r)),
            Fault::Assert { kind, refusal } => write!(f, "assert {kind:?} failed: {}", RefusalText(refusal)),
            Fault::Abort => write!(f, "the program aborts (`terminate`)"),
        }
    }
}

impl core::error::Error for EvalError<'_> {}

/// A refusal's text, as the old evaluator's error prints it (its `Display` is of the whole
/// [`ConstError`]; this prints only the refusal's part).
struct RefusalText<'r, 'a>(&'r Refusal<'a>);

impl fmt::Display for RefusalText<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}
