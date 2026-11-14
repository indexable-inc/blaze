//! blaze's atoms and actions on top of the engine's language (`astlog-engine`): code patterns
//! (`expr`, `type`, `pat`, `stmt`, `item`, `path`), `syntax`, the semantic relations, and the
//! `rename`, `retarget`, `add-use`, `add-param`, `remove-param` and `reorder-params` actions.
//! Templates splice `$var`.

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
    /// A def's own name, the last segment of its path.
    DefName,
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
    /// `(match-arm m a p b)`: a `match`, one of its arms, the arm's pattern and its expression.
    MatchArm,
    /// `(child p c)`: each child node of `p`, in order.
    Child,
    /// `(descendant p c)`: each node below `p`, in order.
    Descendant,
    /// `(int-value n i)`: the integer an integer literal, or a negated one, means.
    IntValue,
    /// `(str-value n s)`: the text a string literal means, unescaped.
    StrValue,
    /// `(def-parent d p)`: the def that owns `d`, or each def `p` owns.
    DefParent,
    /// `(variant-count e n)`: how many variants the enum `e` has.
    VariantCount,
    /// `(type-def t d)`: the ADT a type is, or the trait of a `dyn Trait`.
    TypeDef,
    /// `(type-kind t k)`: the type's shape, as text (`struct`, `ref`, `int`, ...).
    TypeKind,
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
    /// `(add-param d "name: Type" "default")`, or with an index last: the fn `d` takes a new
    /// parameter at the end or before the parameter of that index (`self` not counted), and
    /// every call passes `default` for it.
    AddParam,
    /// `(remove-param d "name")`: the parameter, and its argument at every call.
    RemoveParam,
    /// `(reorder-params d "b" "a")`: every parameter of `d`, in the new order, and every
    /// call's arguments with them.
    ReorderParams,
}

/// The actions and the arities each takes.
const ACTIONS: &[(&str, Action, usize, usize)] = &[
    ("rename", Action::Rename, 2, 2),
    ("retarget", Action::Retarget, 2, 2),
    ("add-use", Action::AddUse, 2, 2),
    ("add-param", Action::AddParam, 3, 4),
    ("remove-param", Action::RemoveParam, 2, 2),
    ("reorder-params", Action::ReorderParams, 2, usize::MAX),
];

const PATTERN_FORMS: &str = "(expr n \"...\"), (type n \"...\"), (pat n \"...\"), (stmt n \"...\"), \
                             (item n \"...\") or (path n \"...\")";

/// Each relation's arity and the arguments it is demand-driven on.
fn shape(head: &str) -> Option<(Atom, usize, Needs)> {
    let all0 = Needs::All(&[0]);
    Some(match head {
        "resolves" => (Atom::Resolves, 2, all0),
        "def-path" => (Atom::DefPath, 2, Needs::Any(&[0, 1])),
        "def-name" => (Atom::DefName, 2, all0),
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
        "match-arm" => (Atom::MatchArm, 4, Needs::Nothing),
        "child" => (Atom::Child, 2, all0),
        "descendant" => (Atom::Descendant, 2, all0),
        "int-value" => (Atom::IntValue, 2, all0),
        "str-value" => (Atom::StrValue, 2, all0),
        "def-parent" => (Atom::DefParent, 2, Needs::Any(&[0, 1])),
        "variant-count" => (Atom::VariantCount, 2, all0),
        "type-def" => (Atom::TypeDef, 2, all0),
        "type-kind" => (Atom::TypeKind, 2, all0),
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
            || ACTIONS.iter().any(|(action, ..)| *action == name)
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
        if head == "def-path"
            && let [_, Sexpr::Str { text, .. }] = args
            && names_an_impl(text)
        {
            return Err(impl_has_no_path(text, line));
        }
        Ok(Some(Parsed { value, args: astlog_engine::terms(args)?, needs }))
    }

    fn parse_action(head: &str, args: &[Sexpr], line: usize) -> Result<Option<Parsed<Action>>, Error> {
        let Some(&(_, value, least, most)) = ACTIONS.iter().find(|(name, ..)| *name == head) else {
            return Ok(None);
        };
        if least == most && args.len() != least {
            return BuiltinAritySnafu { line, name: head, expected: least, got: args.len() }.fail();
        }
        if args.len() < least || args.len() > most {
            let most = if most == usize::MAX { "more".to_owned() } else { most.to_string() };
            return DslSnafu {
                line,
                message: format!("`{head}` takes {least} or {most} arguments, got {}", args.len()),
            }
            .fail();
        }
        // An action's text is the name, path or code itself, no template: `(rename d "$t")`
        // would rename to the text `$t`. A variable is passed bare.
        for arg in args {
            if let Sexpr::Str { text, .. } = arg
                && let Some(var) = dollar_var(text)
            {
                return DslSnafu {
                    line,
                    message: format!(
                        "`{head}` takes its text as written, and `\"{text}\"` is no template: pass \
                         the variable itself, `{var}` without quotes, and compute new text with \
                         text-replace or case"
                    ),
                }
                .fail();
            }
        }
        Ok(Some(Parsed { value, args: astlog_engine::terms(args)?, needs: Needs::Nothing }))
    }
}

/// The first `$name` or `${name}` in `text`, as a template would read it.
fn dollar_var(text: &str) -> Option<&str> {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices('$').find_map(|(at, _)| {
        let rest = &text[at + 1..];
        let inner = rest.strip_prefix('{').unwrap_or(rest);
        let end = inner.find(|c: char| !ident(c)).unwrap_or(inner.len());
        (end > 0).then(|| &inner[..end])
    })
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

/// Whether `path` ends in an impl's segment (`lib::<impl Trait for Type>`), the text
/// `def-path` gives an impl: it names the impl to a reader and resolves to nothing.
pub(crate) fn names_an_impl(path: &str) -> bool {
    path.trim().starts_with("<impl ") || path.contains("::<impl ")
}

/// The error for a def path that names an impl, which no path resolves to.
pub(crate) fn impl_has_no_path(path: &str, line: usize) -> Error {
    SourceError::DefPath {
        line,
        path: path.to_owned(),
        message: "an impl has no path that names it back: bind it from its syntax, \
                  (syntax n \"IMPL\") (defines n d), or from code in it, (enclosing-impl n d)"
            .to_owned(),
    }
    .into()
}

/// The syntax kind named `name` as the parser spells it (`METHOD_CALL_EXPR`).
fn kind_named(name: &str) -> Option<SyntaxKind> {
    (0..SyntaxKind::__LAST as u16)
        .map(SyntaxKind::from)
        .find(|kind| format!("{kind:?}") == name)
}
