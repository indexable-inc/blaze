use snafu::Snafu;

/// Every way loading or evaluating a rules program can fail; `S` is the fact source's own error.
///
/// Displays never embed `{source}`: callers walk the chain.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error<S: std::error::Error + 'static> {
    /// The fact source's own failure: reading files, a query its parser refuses.
    #[snafu(transparent)]
    Source { source: S },

    #[snafu(display("rules:{line}: {message}"))]
    Dsl { line: usize, message: String },

    #[snafu(display(
        "rules:{line}: this `(` is nested past {max} lists, the deepest a rules file may nest"
    ))]
    TooDeep { line: usize, max: usize },

    #[snafu(display("rules:{line}: invalid regex `{pattern}` in text-match"))]
    Regex { line: usize, pattern: String, source: regex::Error },

    #[snafu(display("rules:{line}: head variable `{var}` is not bound by the body"))]
    UnboundHeadVar { line: usize, var: String },

    #[snafu(display("rules:{line}: builtin `{name}` needs argument `{arg}` bound at this point"))]
    UnboundBuiltinArg { line: usize, name: String, arg: String },

    #[snafu(display(
        "rules:{line}: `{name}` needs `{arg}` bound by an atom to its left; it is demand-driven and \
         never enumerates every value"
    ))]
    UnboundSourceArg { line: usize, name: String, arg: String },

    #[snafu(display("rules:{line}: builtin `{name}` takes {expected} arguments, got {got}"))]
    BuiltinArity { line: usize, name: String, expected: usize, got: usize },

    #[snafu(display("rules:{line}: builtin `{name}` needs a node, but `{arg}` is {what}"))]
    BuiltinNotNode { line: usize, name: String, arg: String, what: &'static str },

    #[snafu(display("rules:{line}: builtin `{name}` needs a number, but `{arg}` is {what}"))]
    BuiltinNotNumber { line: usize, name: String, arg: String, what: &'static str },

    #[snafu(display("rules:{line}: relation `{name}` is not defined by any rule"))]
    UnknownRelation { name: String, line: usize },

    #[snafu(display(
        "rules:{line}: relation `{name}` used with arity {got}, defined with arity {expected}"
    ))]
    ArityMismatch { name: String, expected: usize, got: usize, line: usize },

    #[snafu(display("rewrite `{name}`: target `{var}` is bound to {what}, not a node"))]
    ReplaceNotNode { name: String, var: String, what: &'static str },

    #[snafu(display("rules:{line}: template references unbound variable `{var}`"))]
    TemplateVar { var: String, line: usize },

    #[snafu(display("rules:{line}: lint severity must be `error` or `warning`, got `{got}`"))]
    LintSeverity { got: String, line: usize },

    #[snafu(display(
        "rules:{line}: lint message references `{var}`, which is not a head variable of `{relation}`"
    ))]
    LintVar { relation: String, var: String, line: usize },

    #[snafu(display(
        "lint `{rule}` (rules:{line}): relation row has no node-valued column to locate the finding"
    ))]
    LintNoNode { rule: String, line: usize },

    #[snafu(display(
        "rules:{line}: variable `{var}` is used only inside `(not ...)` or `(count ...)`; \
         they can only filter variables a positive atom already binds"
    ))]
    UnsafeNegation { var: String, line: usize },

    #[snafu(display(
        "rules:{line}: negation through recursion has no stratification: {cycle} depend on each \
         other through `(not ...)` or `(count ...)`"
    ))]
    UnstratifiableProgram { cycle: String, line: usize },

    #[snafu(display(
        "rules:{line}: the rule for `{relation}` takes the run past {cap} rows; narrow what it \
         joins"
    ))]
    TooManyRows { relation: String, line: usize, cap: usize },

    #[snafu(display(
        "rules:{line}: this source atom's answers take the run past {cap} rows; bind more of its \
         arguments before it"
    ))]
    TooManySourceRows { line: usize, cap: usize },

    #[snafu(display(
        "rules:{line}: the rewrite's bindings take the run past {cap} rows; narrow its body"
    ))]
    TooManyBindings { line: usize, cap: usize },

    #[snafu(display("internal invariant broken: {what}"))]
    Internal { what: String },
}
