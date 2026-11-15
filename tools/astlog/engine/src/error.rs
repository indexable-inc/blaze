use snafu::Snafu;

use crate::line::Line;

/// Every way loading or evaluating a rules program can fail; `S` is the fact source's own error.
///
/// Each `line` names its file: `rules:N` for the request's own text, the included file's key
/// for a form read through `(include ...)`. Displays never embed `{source}`: callers walk the
/// chain.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error<S: std::error::Error + 'static> {
    /// The fact source's own failure: reading files, a query its parser refuses.
    #[snafu(transparent)]
    Source { source: S },

    #[snafu(display("{line}: {message}"))]
    Dsl { line: Line, message: String },

    #[snafu(display(
        "{line}: this `(` is nested past {max} lists, the deepest a rules file may nest"
    ))]
    TooDeep { line: Line, max: usize },

    #[snafu(display("{line}: `(include \"{path}\")`: {why}"))]
    IncludePath { line: Line, path: String, why: &'static str },

    #[snafu(display(
        "{line}: `(include \"{path}\")`: this rules text has no file of its own to read an include \
         beside"
    ))]
    IncludeNowhere { line: Line, path: String },

    #[snafu(display("{line}: `(include \"{path}\")`: there is no file at `{key}`"))]
    IncludeMissing { line: Line, path: String, key: String },

    #[snafu(display("{line}: `(include \"{path}\")`: `{key}` cannot be read as text"))]
    IncludeRead { line: Line, path: String, key: String, source: std::io::Error },

    #[snafu(display("{line}: `(include \"{path}\")` closes a cycle of includes: {cycle}"))]
    IncludeCycle { line: Line, path: String, cycle: String },

    #[snafu(display(
        "{line}: `(include \"{path}\")` is past {max} included files, the most one rules text may \
         read"
    ))]
    TooManyIncludes { line: Line, path: String, max: usize },

    #[snafu(display(
        "{line}: `(include \"{path}\")` takes the rules past {max} bytes of text, the most one \
         load reads"
    ))]
    TooMuchRules { line: Line, path: String, max: usize },

    #[snafu(display("{line}: invalid regex `{pattern}` in {name}"))]
    Regex { line: Line, name: String, pattern: String, source: regex::Error },

    #[snafu(display("{line}: head variable `{var}` is not bound by the body"))]
    UnboundHeadVar { line: Line, var: String },

    #[snafu(display("{line}: builtin `{name}` needs argument `{arg}` bound at this point"))]
    UnboundBuiltinArg { line: Line, name: String, arg: String },

    #[snafu(display(
        "{line}: `{name}` needs `{arg}` bound by an atom to its left; it is demand-driven and \
         never enumerates every value"
    ))]
    UnboundSourceArg { line: Line, name: String, arg: String },

    #[snafu(display("{line}: builtin `{name}` takes {expected} arguments, got {got}"))]
    BuiltinArity { line: Line, name: String, expected: usize, got: usize },

    #[snafu(display("{line}: builtin `{name}` needs a node, but `{arg}` is {what}"))]
    BuiltinNotNode { line: Line, name: String, arg: String, what: &'static str },

    #[snafu(display("{line}: builtin `{name}` needs a number, but `{arg}` is {what}"))]
    BuiltinNotNumber { line: Line, name: String, arg: String, what: &'static str },

    #[snafu(display(
        "{line}: `case` reads an identifier's words, but `{text}` is not an identifier \
         (letters, digits, `_` and `-` only)"
    ))]
    CaseNotIdentifier { line: Line, text: String },

    #[snafu(display("{line}: relation `{name}` is not defined by any rule"))]
    UnknownRelation { name: String, line: Line },

    #[snafu(display(
        "{line}: relation `{name}` used with arity {got}, defined with arity {expected}"
    ))]
    ArityMismatch { name: String, expected: usize, got: usize, line: Line },

    #[snafu(display("rewrite `{name}`: target `{var}` is bound to {what}, not a node"))]
    ReplaceNotNode { name: String, var: String, what: &'static str },

    #[snafu(display("{line}: template references unbound variable `{var}`"))]
    TemplateVar { var: String, line: Line },

    #[snafu(display("{line}: lint severity must be `error` or `warning`, got `{got}`"))]
    LintSeverity { got: String, line: Line },

    #[snafu(display(
        "{line}: lint message references `{var}`, which is not a head variable of `{relation}`"
    ))]
    LintVar { relation: String, var: String, line: Line },

    #[snafu(display(
        "lint `{rule}` ({line}): relation row has no node-valued column to locate the finding"
    ))]
    LintNoNode { rule: String, line: Line },

    #[snafu(display(
        "{line}: variable `{var}` is used only inside `(not ...)` or `(count ...)`; \
         they can only filter variables a positive atom already binds"
    ))]
    UnsafeNegation { var: String, line: Line },

    #[snafu(display(
        "{line}: negation through recursion has no stratification: {cycle} depend on each \
         other through `(not ...)` or `(count ...)`"
    ))]
    UnstratifiableProgram { cycle: String, line: Line },

    #[snafu(display(
        "{line}: the rule for `{relation}` takes the run past {cap} rows; narrow what it joins"
    ))]
    TooManyRows { relation: String, line: Line, cap: usize },

    #[snafu(display(
        "{line}: this source atom's answers take the run past {cap} rows; bind more of its \
         arguments before it"
    ))]
    TooManySourceRows { line: Line, cap: usize },

    #[snafu(display("{line}: the rewrite's bindings take the run past {cap} rows; narrow its body"))]
    TooManyBindings { line: Line, cap: usize },

    #[snafu(display("internal invariant broken: {what}"))]
    Internal { what: String },
}
