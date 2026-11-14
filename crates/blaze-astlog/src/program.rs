//! blaze's atoms and actions on top of the engine's language (`astlog-engine`): code patterns
//! (`expr`, `type`, `pat`, `stmt`, `item`, `path`), `syntax`, the semantic relations, and the
//! `rename`, `retarget` and `add-use` actions. Templates splice `$var`.

use astlog_engine::{BuiltinAritySnafu, DslSnafu, Needs, Parsed, Sexpr, TemplateSyntax, Term};
use syntax::SyntaxKind;

use crate::error::{Error, SourceError};
use crate::pattern::{Form, Pattern, TypePattern};

/// blaze's fact source, as the language sees it.
#[derive(Debug)]
pub struct Blaze;

/// A checked program over blaze's atoms.
pub type Program = astlog_engine::Program<Blaze>;

/// Which item `enclosing-*` finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enclosing {
    Fn,
    Impl,
    Module,
}

/// Which filter a one-argument relation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    InMacroInput,
    InMacroExpansion,
    InUnsafe,
    InTest,
    CfgOff,
}

#[derive(Debug)]
pub enum Atom {
    Pattern(Pattern),
    Syntax(SyntaxKind),
    Resolves,
    DefPath,
    TypeOf,
    /// A type with every leading `&` and `&mut` stripped.
    PeelRefs,
    TypeMatches(TypePattern),
    /// The trait's def path, as written.
    Implements(String),
    Callee,
    Enclosing(Enclosing),
    CrateOf,
    FnParam,
    FnReturn,
    DefKind,
    Visibility,
    /// The attribute's name (its path's first segment).
    Attribute(String),
    Defines,
    References,
    Filter(Filter),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `(rename d "new")`: the def and every reference, through blaze-intent.
    Rename,
    /// `(retarget n "krate::path")`: the path node `n` names that item instead, written as its
    /// module sees it.
    Retarget,
    /// `(add-use n "krate::path")`: a `use` of the item in the file (or inline module) of `n`.
    AddUse,
}

const PATTERN_FORMS: &str = "(expr n \"...\"), (type n \"...\"), (pat n \"...\"), (stmt n \"...\"), \
                             (item n \"...\") or (path n \"...\")";

/// Each relation's arity and the arguments it is demand-driven on.
fn shape(head: &str) -> Option<(Atom, usize, Needs)> {
    let all0 = Needs::All(&[0]);
    Some(match head {
        "resolves" => (Atom::Resolves, 2, all0),
        "def-path" => (Atom::DefPath, 2, Needs::Any(&[0, 1])),
        "type-of" => (Atom::TypeOf, 2, all0),
        "peel-refs" => (Atom::PeelRefs, 2, all0),
        "callee" => (Atom::Callee, 2, all0),
        "enclosing-fn" => (Atom::Enclosing(Enclosing::Fn), 2, all0),
        "enclosing-impl" => (Atom::Enclosing(Enclosing::Impl), 2, all0),
        "enclosing-module" => (Atom::Enclosing(Enclosing::Module), 2, all0),
        "crate-of" => (Atom::CrateOf, 2, all0),
        "fn-param" => (Atom::FnParam, 3, all0),
        "fn-return" => (Atom::FnReturn, 2, all0),
        "def-kind" => (Atom::DefKind, 2, all0),
        "visibility" => (Atom::Visibility, 2, all0),
        "defines" => (Atom::Defines, 2, Needs::Any(&[0, 1])),
        "references" => (Atom::References, 2, all0),
        "in-macro-input" => (Atom::Filter(Filter::InMacroInput), 1, all0),
        "in-macro-expansion" => (Atom::Filter(Filter::InMacroExpansion), 1, all0),
        "in-unsafe" => (Atom::Filter(Filter::InUnsafe), 1, all0),
        "in-test" => (Atom::Filter(Filter::InTest), 1, all0),
        "cfg-off" => (Atom::Filter(Filter::CfgOff), 1, all0),
        _ => return None,
    })
}

fn form_of(head: &str) -> Option<Form> {
    Some(match head {
        "expr" => Form::Expr,
        "type" => Form::Type,
        "pat" => Form::Pat,
        "stmt" => Form::Stmt,
        "item" => Form::Item,
        "path" => Form::Path,
        _ => return None,
    })
}

const LITERAL_ATOMS: &[&str] = &["syntax", "type-matches", "implements", "attribute"];

impl astlog_engine::Language for Blaze {
    type Atom = Atom;
    type Action = Action;
    type Error = SourceError;
    const TEMPLATE: TemplateSyntax = TemplateSyntax::Dollar;

    fn claims(name: &str) -> bool {
        name == "match"
            || form_of(name).is_some()
            || shape(name).is_some()
            || LITERAL_ATOMS.contains(&name)
            || matches!(name, "rename" | "retarget" | "add-use")
    }

    fn parse_atom(head: &str, args: &[Sexpr], line: usize) -> Result<Option<Parsed<Atom>>, Error> {
        if head == "match" {
            return DslSnafu {
                line,
                message: format!(
                    "(match ...) is a tree-sitter query, which blaze's rules do not run: write a \
                     Rust pattern, {PATTERN_FORMS}"
                ),
            }
            .fail();
        }
        if let Some(form) = form_of(head) {
            let (term, text) = term_and_literal(head, args, line)?;
            let pattern = Pattern::parse(form, text, line)?;
            let mut terms = vec![term];
            terms.extend(pattern.vars.iter().map(|var| Term::Var(var.name.clone())));
            return Ok(Some(Parsed { value: Atom::Pattern(pattern), args: terms, needs: Needs::Nothing }));
        }
        match head {
            "syntax" => {
                let (term, text) = term_and_literal(head, args, line)?;
                let kind = kind_named(text)
                    .ok_or_else(|| SourceError::UnknownKind { line, kind: text.to_owned() })?;
                return Ok(Some(Parsed {
                    value: Atom::Syntax(kind),
                    args: vec![term],
                    needs: Needs::Nothing,
                }));
            }
            "type-matches" => {
                let (term, text) = term_and_literal(head, args, line)?;
                let pattern = TypePattern::parse(text, line)?;
                let mut terms = vec![term];
                terms.extend(pattern.vars.iter().map(|var| Term::Var(var.clone())));
                return Ok(Some(Parsed {
                    value: Atom::TypeMatches(pattern),
                    args: terms,
                    needs: Needs::All(&[0]),
                }));
            }
            "implements" | "attribute" => {
                let (term, text) = term_and_literal(head, args, line)?;
                let value = match head {
                    "implements" => Atom::Implements(text.to_owned()),
                    _ => Atom::Attribute(text.to_owned()),
                };
                return Ok(Some(Parsed { value, args: vec![term], needs: Needs::All(&[0]) }));
            }
            _ => {}
        }
        let Some((value, arity, needs)) = shape(head) else { return Ok(None) };
        if args.len() != arity {
            return BuiltinAritySnafu { line, name: head, expected: arity, got: args.len() }.fail();
        }
        Ok(Some(Parsed { value, args: astlog_engine::terms(args)?, needs }))
    }

    fn parse_action(head: &str, args: &[Sexpr], line: usize) -> Result<Option<Parsed<Action>>, Error> {
        let value = match head {
            "rename" => Action::Rename,
            "retarget" => Action::Retarget,
            "add-use" => Action::AddUse,
            _ => return Ok(None),
        };
        if args.len() != 2 {
            return BuiltinAritySnafu { line, name: head, expected: 2usize, got: args.len() }.fail();
        }
        Ok(Some(Parsed { value, args: astlog_engine::terms(args)?, needs: Needs::Nothing }))
    }
}

/// `(head term "literal")`: the term, and the literal the atom is compiled from.
fn term_and_literal<'a>(head: &str, args: &'a [Sexpr], line: usize) -> Result<(Term, &'a str), Error> {
    let [first, Sexpr::Str { text, .. }] = args else {
        return DslSnafu {
            line,
            message: format!("`{head}` takes a variable and a string literal: ({head} v \"...\")"),
        }
        .fail();
    };
    let term = astlog_engine::terms(std::slice::from_ref(first))?.remove(0);
    Ok((term, text))
}

/// The syntax kind named `name` as the parser spells it (`METHOD_CALL_EXPR`).
fn kind_named(name: &str) -> Option<SyntaxKind> {
    (0..SyntaxKind::__LAST as u16)
        .map(SyntaxKind::from)
        .find(|kind| format!("{kind:?}") == name)
}
