//! astlog's language and evaluator, generic over a fact source.
//!
//! A rules file is read ([`sexpr`]), checked into a [`Program`] over a [`Language`]'s atoms, and
//! run to a fixpoint against that source's [`Facts`] by the semi-naive [`Evaluator`]. Rewrites
//! become [`Planned`] actions the source turns into edits; lints become rows the source locates.
//! The tree-sitter source is `astlog-core`; blaze's own front end is `blaze-astlog`. See
//! `tools/astlog/README.md` for the language.

mod error;
mod eval;
mod program;
mod sexpr;

#[cfg(test)]
mod naive;
#[cfg(test)]
mod tests;

use std::alloc::Global;

pub use crate::error::*;
pub use crate::eval::{Binding, Database, Evaluator, Facts, Relation, Row, Value};
pub use crate::program::{
    Action, AppAtom, BodyAtom, CountAtom, Language, Lint, Needs, Parsed, Program, Rewrite, Rule,
    Segment, Severity, SourceAction, SourceAtom, Template, TemplateSyntax, Term, TextAction,
    TextKind, parse_template, terms,
};
pub use crate::sexpr::Sexpr;

type FError<F> = Error<<<F as Facts>::Lang as Language>::Error>;

/// One action of one rewrite for one binding, its templates rendered.
#[derive(Debug)]
pub struct Planned<'p, N, X> {
    /// The rewrite's name.
    pub rule: &'p str,
    pub line: usize,
    pub action: PlannedAction<'p, N, X>,
}

#[derive(Debug)]
pub enum PlannedAction<'p, N, X> {
    Text { kind: TextKind, node: N, text: String },
    Source { name: &'p str, value: &'p X, args: Vec<Value<N>> },
}

/// Run `program` against `facts`: the derived relations and every rewrite action, in rewrite
/// order and then binding order.
///
/// # Errors
///
/// A source's failure, a builtin given the wrong kind of value, or a rewrite target bound to
/// something other than a node.
#[expect(clippy::type_complexity, reason = "the two results of one run, named by the signature")]
pub fn run<'p, F: Facts>(
    program: &'p Program<F::Lang>,
    facts: &'p F,
) -> Result<(Database<F::Node>, Vec<Planned<'p, F::Node, <F::Lang as Language>::Action>>), FError<F>>
{
    let mut evaluator = Evaluator::new(program, facts)?;
    evaluator.fixpoint()?;
    let mut planned = Vec::new();
    for rewrite in &program.rewrites {
        for binding in evaluator.bindings(&rewrite.body)? {
            for action in &rewrite.actions {
                planned.push(plan(facts, rewrite, action, &binding)?);
            }
        }
    }
    Ok((evaluator.into_database(), planned))
}

fn plan<'p, F: Facts>(
    facts: &F,
    rewrite: &'p Rewrite<<F::Lang as Language>::Atom, <F::Lang as Language>::Action>,
    action: &'p Action<<F::Lang as Language>::Action>,
    binding: &Binding<F::Node>,
) -> Result<Planned<'p, F::Node, <F::Lang as Language>::Action>, FError<F>> {
    let action = match action {
        Action::Text(text) => {
            let target = binding.get(&text.target).ok_or_else(|| {
                TemplateVarSnafu { var: text.target.clone(), line: text.line }.build()
            })?;
            let Value::Node(node) = target else {
                return ReplaceNotNodeSnafu {
                    name: rewrite.name.clone(),
                    var: text.target.clone(),
                    what: target.what(),
                }
                .fail();
            };
            PlannedAction::Text {
                kind: text.kind,
                node: *node,
                text: render(facts, &text.template, binding)?,
            }
        }
        Action::Source(source) => {
            let args = source
                .args
                .iter()
                .map(|term| match term {
                    Term::Var(var) => binding.get(var).cloned().ok_or_else(|| {
                        TemplateVarSnafu { var: var.clone(), line: source.line }.build()
                    }),
                    Term::Text(text) => Ok(Value::Text(std::sync::Arc::from(text.as_str()))),
                    Term::Int(int) => Ok(Value::Int(*int)),
                })
                .collect::<Result<_, FError<F>>>()?;
            PlannedAction::Source { name: &source.name, value: &source.value, args }
        }
    };
    Ok(Planned { rule: &rewrite.name, line: rewrite.line, action })
}

/// A template with each variable replaced by its value's text.
///
/// # Errors
///
/// [`Error::TemplateVar`] for a variable the binding lacks.
pub fn render<F: Facts>(
    facts: &F,
    template: &Template,
    binding: &Binding<F::Node>,
) -> Result<String, FError<F>> {
    let mut out = String::new();
    for segment in &template.segments {
        match segment {
            Segment::Lit(lit) => out.push_str(lit),
            Segment::Var(var) => {
                let value = binding.get(var).ok_or_else(|| {
                    TemplateVarSnafu { var: var.clone(), line: template.line }.build()
                })?;
                out.push_str(&facts.value_text(value));
            }
        }
    }
    Ok(out)
}

/// One relation row as a binding of its columns, for rendering a lint message.
#[must_use]
pub fn row_binding<N: Clone>(columns: &[String], row: &[Value<N>]) -> Binding<N> {
    let mut binding = Binding::new_in(Global);
    for (column, value) in columns.iter().zip(row) {
        binding.insert(column.clone(), value.clone());
    }
    binding
}
