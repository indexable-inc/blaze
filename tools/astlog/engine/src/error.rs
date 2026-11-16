use std::fmt;

/// Every way loading or evaluating a rules program can fail; `S` is the fact source's own error.
///
/// Displays never embed the source error: callers walk the chain.
#[derive(Debug)]
pub enum Error<S: std::error::Error + 'static> {
    /// The fact source's own failure: reading files, a query its parser refuses. Displays as the
    /// source error itself.
    Source { source: S },

    Dsl { line: usize, message: String },

    TooDeep { line: usize, max: usize },

    Regex { line: usize, name: String, pattern: String, source: blaze_script::regex::Error },

    UnboundHeadVar { line: usize, var: String },

    UnboundBuiltinArg { line: usize, name: String, arg: String },

    UnboundSourceArg { line: usize, name: String, arg: String },

    BuiltinArity { line: usize, name: String, expected: usize, got: usize },

    BuiltinNotNode { line: usize, name: String, arg: String, what: &'static str },

    BuiltinNotNumber { line: usize, name: String, arg: String, what: &'static str },

    CaseNotIdentifier { line: usize, text: String },

    UnknownRelation { name: String, line: usize },

    ArityMismatch { name: String, expected: usize, got: usize, line: usize },

    ReplaceNotNode { name: String, var: String, what: &'static str },

    TemplateVar { var: String, line: usize },

    LintSeverity { got: String, line: usize },

    LintVar { relation: String, var: String, line: usize },

    LintNoNode { rule: String, line: usize },

    UnsafeNegation { var: String, line: usize },

    UnstratifiableProgram { cycle: String, line: usize },

    TooManyRows { relation: String, line: usize, cap: usize },

    TooManySourceRows { line: usize, cap: usize },

    TooManyBindings { line: usize, cap: usize },

    Internal { what: String },
}

impl<S: std::error::Error + 'static> From<S> for Error<S> {
    fn from(source: S) -> Self {
        Self::Source { source }
    }
}

impl<S: std::error::Error + 'static> fmt::Display for Error<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source { source } => fmt::Display::fmt(source, f),
            Self::Dsl { line, message } => write!(f, "rules:{line}: {message}"),
            Self::TooDeep { line, max } => write!(
                f,
                "rules:{line}: this `(` is nested past {max} lists, the deepest a rules file may nest"
            ),
            Self::Regex { line, name, pattern, .. } => {
                write!(f, "rules:{line}: invalid regex `{pattern}` in {name}")
            }
            Self::UnboundHeadVar { line, var } => {
                write!(f, "rules:{line}: head variable `{var}` is not bound by the body")
            }
            Self::UnboundBuiltinArg { line, name, arg } => write!(
                f,
                "rules:{line}: builtin `{name}` needs argument `{arg}` bound at this point"
            ),
            Self::UnboundSourceArg { line, name, arg } => write!(
                f,
                "rules:{line}: `{name}` needs `{arg}` bound by an atom to its left; it is \
                 demand-driven and never enumerates every value"
            ),
            Self::BuiltinArity { line, name, expected, got } => write!(
                f,
                "rules:{line}: builtin `{name}` takes {expected} arguments, got {got}"
            ),
            Self::BuiltinNotNode { line, name, arg, what } => write!(
                f,
                "rules:{line}: builtin `{name}` needs a node, but `{arg}` is {what}"
            ),
            Self::BuiltinNotNumber { line, name, arg, what } => write!(
                f,
                "rules:{line}: builtin `{name}` needs a number, but `{arg}` is {what}"
            ),
            Self::CaseNotIdentifier { line, text } => write!(
                f,
                "rules:{line}: `case` reads an identifier's words, but `{text}` is not an \
                 identifier (letters, digits, `_` and `-` only)"
            ),
            Self::UnknownRelation { name, line } => {
                write!(f, "rules:{line}: relation `{name}` is not defined by any rule")
            }
            Self::ArityMismatch { name, expected, got, line } => write!(
                f,
                "rules:{line}: relation `{name}` used with arity {got}, defined with arity \
                 {expected}"
            ),
            Self::ReplaceNotNode { name, var, what } => {
                write!(f, "rewrite `{name}`: target `{var}` is bound to {what}, not a node")
            }
            Self::TemplateVar { var, line } => {
                write!(f, "rules:{line}: template references unbound variable `{var}`")
            }
            Self::LintSeverity { got, line } => {
                write!(f, "rules:{line}: lint severity must be `error` or `warning`, got `{got}`")
            }
            Self::LintVar { relation, var, line } => write!(
                f,
                "rules:{line}: lint message references `{var}`, which is not a head variable of \
                 `{relation}`"
            ),
            Self::LintNoNode { rule, line } => write!(
                f,
                "lint `{rule}` (rules:{line}): relation row has no node-valued column to locate \
                 the finding"
            ),
            Self::UnsafeNegation { var, line } => write!(
                f,
                "rules:{line}: variable `{var}` is used only inside `(not ...)` or `(count ...)`; \
                 they can only filter variables a positive atom already binds"
            ),
            Self::UnstratifiableProgram { cycle, line } => write!(
                f,
                "rules:{line}: negation through recursion has no stratification: {cycle} depend \
                 on each other through `(not ...)` or `(count ...)`"
            ),
            Self::TooManyRows { relation, line, cap } => write!(
                f,
                "rules:{line}: the rule for `{relation}` takes the run past {cap} rows; narrow \
                 what it joins"
            ),
            Self::TooManySourceRows { line, cap } => write!(
                f,
                "rules:{line}: this source atom's answers take the run past {cap} rows; bind more \
                 of its arguments before it"
            ),
            Self::TooManyBindings { line, cap } => write!(
                f,
                "rules:{line}: the rewrite's bindings take the run past {cap} rows; narrow its \
                 body"
            ),
            Self::Internal { what } => write!(f, "internal invariant broken: {what}"),
        }
    }
}

impl<S: std::error::Error + 'static> std::error::Error for Error<S> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // Transparent: the source error displays as this one, so its own source is next.
            Self::Source { source } => source.source(),
            Self::Regex { source, .. } => Some(source),
            _ => None,
        }
    }
}
