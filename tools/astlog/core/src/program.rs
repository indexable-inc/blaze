//! The tree-sitter source's atoms, on top of the engine's language (`astlog-engine`).
//!
//! `(match <lang> "<tree-sitter query>")` binds every `@capture` in the query as a variable.
//! `(no-descendant <node> "<kind>" "<text>")` and `(attached-sibling <node> "<kind>" "<text>")`
//! walk this source's preorder node table. Templates splice bound variables with `{var}`; `{{`
//! and `}}` escape literal braces.

use ast_merge_langs::Lang;
use astlog_engine::{BuiltinAritySnafu, DslSnafu, Needs, Parsed, Sexpr, TemplateSyntax, Term};

use crate::error::{Error, SourceError, UnknownLangNameSnafu};

/// The tree-sitter fact source, as the language sees it.
#[derive(Debug)]
pub struct TreeSitter;

/// The checked program over tree-sitter atoms.
pub type Program = astlog_engine::Program<TreeSitter>;

#[derive(Debug)]
pub enum Atom {
    Match(MatchAtom),
    NoDescendant,
    AttachedSibling,
}

#[derive(Debug)]
pub struct MatchAtom {
    pub lang: Lang,
    pub query: String,
    /// The query's capture names, each once, in first-appearance order: the atom's columns.
    pub captures: Vec<String>,
}

/// This source adds no actions beyond the engine's text actions.
#[derive(Debug)]
pub enum Action {}

impl astlog_engine::Language for TreeSitter {
    type Atom = Atom;
    type Action = Action;
    type Error = SourceError;
    const TEMPLATE: TemplateSyntax = TemplateSyntax::Braces;

    fn claims(name: &str) -> bool {
        matches!(name, "match" | "no-descendant" | "attached-sibling")
    }

    fn parse_atom(head: &str, args: &[Sexpr], line: usize) -> Result<Option<Parsed<Atom>>, Error> {
        match head {
            "match" => parse_match(args, line).map(Some),
            "no-descendant" | "attached-sibling" => {
                if args.len() != 3 {
                    return BuiltinAritySnafu {
                        line,
                        name: head,
                        expected: 3usize,
                        got: args.len(),
                    }
                    .fail();
                }
                let value = if head == "no-descendant" {
                    Atom::NoDescendant
                } else {
                    Atom::AttachedSibling
                };
                Ok(Some(Parsed {
                    value,
                    args: astlog_engine::terms(args)?,
                    needs: Needs::All(&[0, 1, 2]),
                }))
            }
            _ => Ok(None),
        }
    }

    fn parse_action(_: &str, _: &[Sexpr], _: usize) -> Result<Option<Parsed<Action>>, Error> {
        Ok(None)
    }
}

fn parse_match(args: &[Sexpr], line: usize) -> Result<Parsed<Atom>, Error> {
    let [Sexpr::Atom { text: lang_name, line: lang_line }, Sexpr::Str { text: query, .. }] = args
    else {
        return DslSnafu { line, message: "match form is (match <lang> \"<query>\")" }.fail();
    };
    let mut captures: Vec<String> = Vec::new();
    for name in capture_names(query) {
        if !captures.iter().any(|seen| seen == name) {
            captures.push(name.to_owned());
        }
    }
    let atom_args = captures.iter().map(|name| Term::Var(name.clone())).collect();
    let lang = resolve_lang(lang_name, *lang_line)?;
    Ok(Parsed {
        value: Atom::Match(MatchAtom { lang, query: query.clone(), captures }),
        args: atom_args,
        needs: Needs::Nothing,
    })
}

/// Resolve a language name as written in `(match <lang> ...)`.
///
/// Accepts the profile name case-insensitively (`rust`, `toml`, `python`) or any registered file
/// extension (`rs`, `py`).
fn resolve_lang(name: &str, line: usize) -> Result<Lang, Error> {
    let lowered = name.to_lowercase();
    for lang in Lang::all() {
        if lang.profile().name.to_lowercase() == lowered {
            return Ok(*lang);
        }
    }
    ast_merge_langs::detect_from_extension(&lowered)
        .ok_or_else(|| UnknownLangNameSnafu { name: name.to_owned(), line }.build().into())
}

/// Capture names mentioned in a tree-sitter query source (`@name`).
///
/// Lexical scan, skipping string literals and `;` comments (which run to end of line); the query
/// is validated for real by `tree_sitter::Query::new` at evaluation setup.
fn capture_names(query: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = query;
    let mut in_string = false;
    let mut chars = rest.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => in_string = !in_string,
            '\\' if in_string => {
                chars.next();
            }
            ';' if !in_string => {
                for (_, c) in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '@' if !in_string => {
                rest = &query[at + 1..];
                let end = rest
                    .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')))
                    .unwrap_or(rest.len());
                if end > 0 {
                    names.push(&rest[..end]);
                }
            }
            _ => {}
        }
    }
    names
}
