//! Recursive-descent parser for `.lark` grammar text, following the
//! meta-grammar in Lark's `load_grammar.py` (`RULES`).

use super::ast::{
    Alternative, Definition, Expr, ImportNames, ImportStmt, Pattern, PatternKind, RuleDef,
    RuleModifiers, Statement, Symbol, TermDef,
};
use super::escape::{range_literal, regex_literal, string_literal};
use super::lexer::{Pos, Tok, Token, tokenize};
use super::{GrammarError, GrammarResult};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

struct Parser<'a> {
    tokens: Vec<Token>,
    i: usize,
    text: &'a str,
    /// How many parentheses and brackets are open around the current
    /// position.
    depth: usize,
}

/// The most repetitions a `~` count may ask for.
const REPEAT_LIMIT: u32 = 1 << 20;

/// How deeply parentheses, brackets and template arguments may nest. The
/// parser and every pass after it walk the tree recursively, so a grammar
/// nested past this is rejected rather than left to overflow the stack.
const MAX_NESTING: usize = 200;

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.i).map(|t| &t.tok)
    }

    fn peek_at(&self, offset: usize) -> Option<&Tok> {
        self.tokens.get(self.i + offset).map(|t| &t.tok)
    }

    fn pos(&self) -> Pos {
        let token = self.tokens.get(self.i).or(self.tokens.last());
        token.map_or(Pos { line: 1, column: 1 }, |t| t.pos)
    }

    fn advance(&mut self) -> Option<Tok> {
        let tok = self.tokens.get(self.i)?.tok.clone();
        self.i += 1;
        Some(tok)
    }

    fn error_at(&self, pos: Pos, what: &str) -> GrammarError {
        let line_text = self.text.lines().nth(pos.line as usize - 1).unwrap_or("");
        GrammarError::new(format!(
            "{what}, at line {} column {}\n\n{line_text}",
            pos.line, pos.column
        ))
    }

    fn error(&self, what: &str) -> GrammarError {
        self.error_at(self.pos(), what)
    }

    fn eat(&mut self, expected: &Tok) -> bool {
        if self.peek() == Some(expected) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &Tok, what: &str) -> GrammarResult<()> {
        if self.eat(expected) {
            Ok(())
        } else {
            Err(self.error(what))
        }
    }

    fn expect_newline(&mut self) -> GrammarResult<()> {
        match self.peek() {
            Some(Tok::Nl) | None => {
                if self.peek().is_some() {
                    self.i += 1;
                }
                Ok(())
            }
            Some(Tok::Colon) => Err(self.error("Unexpected colon")),
            Some(Tok::Op(_)) => Err(self.error("Misplaced operator")),
            Some(Tok::RPar | Tok::RBra) => Err(self.error("Unmatched closing parenthesis")),
            Some(_) => {
                Err(self.error("Expecting option (\"|\") or a new rule or terminal definition"))
            }
        }
    }

    fn parse_grammar(&mut self) -> GrammarResult<Vec<Statement>> {
        let mut statements = Vec::new();
        while self.peek().is_some() {
            if self.eat(&Tok::Nl) || self.eat(&Tok::NlOr) {
                if self.tokens[self.i - 1].tok == Tok::NlOr {
                    return Err(self.error_at(
                        self.tokens[self.i - 1].pos,
                        "Expecting rule or terminal definition",
                    ));
                }
                continue;
            }
            statements.push(self.statement()?);
        }
        Ok(statements)
    }

    fn statement(&mut self) -> GrammarResult<Statement> {
        match self.peek() {
            Some(Tok::RuleModifiers(_) | Tok::Rule(_)) => {
                Ok(Statement::Define(Definition::Rule(self.rule_def()?)))
            }
            Some(Tok::Terminal(_)) => Ok(Statement::Define(Definition::Terminal(self.term_def()?))),
            Some(Tok::Override) => {
                self.advance();
                Ok(Statement::Override(self.definition()?))
            }
            Some(Tok::Extend) => {
                self.advance();
                Ok(Statement::Extend(self.definition()?))
            }
            Some(Tok::Ignore) => {
                self.advance();
                if !self.starts_atom() {
                    return Err(self.error("%ignore expects a value"));
                }
                let expr = self.expansions()?;
                self.expect_newline()?;
                Ok(Statement::Ignore(expr))
            }
            Some(Tok::Declare) => {
                self.advance();
                let mut symbols = Vec::new();
                loop {
                    match self.peek() {
                        Some(Tok::Terminal(name)) => {
                            symbols.push(Symbol::Terminal(name.clone()));
                            self.advance();
                        }
                        Some(Tok::Rule(name)) => {
                            symbols.push(Symbol::Rule(name.clone()));
                            self.advance();
                        }
                        _ => break,
                    }
                }
                if symbols.is_empty() {
                    return Err(self.error("%declare expects at least one name"));
                }
                self.expect_newline()?;
                Ok(Statement::Declare(symbols))
            }
            Some(Tok::Import) => {
                self.advance();
                let stmt = self.import()?;
                self.expect_newline()?;
                Ok(Statement::Import(stmt))
            }
            Some(Tok::Str { .. } | Tok::Regexp { .. }) => {
                Err(self.error("Expecting rule or terminal definition"))
            }
            Some(Tok::RPar | Tok::RBra) => Err(self.error("Unmatched closing parenthesis")),
            Some(Tok::Op(_)) => Err(self.error("Misplaced operator")),
            Some(Tok::LPar | Tok::LBra) => {
                Err(self.error("Expecting option (\"|\") or a new rule or terminal definition"))
            }
            Some(_) | None => Err(self.error("Expecting rule or terminal definition")),
        }
    }

    fn definition(&mut self) -> GrammarResult<Definition> {
        match self.peek() {
            Some(Tok::RuleModifiers(_) | Tok::Rule(_)) => Ok(Definition::Rule(self.rule_def()?)),
            Some(Tok::Terminal(_)) => Ok(Definition::Terminal(self.term_def()?)),
            _ => Err(self.error("Expecting rule or terminal definition")),
        }
    }

    fn name_token(&mut self) -> GrammarResult<String> {
        match self.peek() {
            Some(Tok::Rule(name) | Tok::Terminal(name)) => {
                let name = name.clone();
                self.advance();
                Ok(name)
            }
            _ => Err(self.error("Expecting a name")),
        }
    }

    fn import(&mut self) -> GrammarResult<ImportStmt> {
        let relative = self.eat(&Tok::Dot);
        let mut path = Vec::new();
        if !matches!(self.peek(), Some(Tok::Rule(_) | Tok::Terminal(_))) {
            return Err(self.error("%import expects a name"));
        }
        path.push(self.name_token()?);
        while self.eat(&Tok::Dot) {
            path.push(self.name_token()?);
        }
        let names = if self.eat(&Tok::LPar) {
            let mut names = Vec::new();
            names.push(self.name_token()?);
            while self.eat(&Tok::Comma) {
                names.push(self.name_token()?);
            }
            self.expect(&Tok::RPar, "Unclosed parenthesis")?;
            ImportNames::Multi(names)
        } else {
            let name = path.pop().unwrap();
            if path.is_empty() {
                return Err(GrammarError::new(format!(
                    "Nothing was imported from grammar `{name}`"
                )));
            }
            let alias = if self.eat(&Tok::To) {
                Some(self.name_token()?)
            } else {
                None
            };
            ImportNames::Single { name, alias }
        };
        Ok(ImportStmt {
            path,
            relative,
            names,
        })
    }

    fn rule_def(&mut self) -> GrammarResult<RuleDef> {
        let mut modifiers = RuleModifiers::default();
        if let Some(Tok::RuleModifiers(m)) = self.peek() {
            modifiers.expand1 = m.contains('?');
            modifiers.keep_all_tokens = m.contains('!');
            self.advance();
        }
        let name = match self.peek() {
            Some(Tok::Rule(name)) => name.clone(),
            _ => return Err(self.error("Expecting rule or terminal definition")),
        };
        self.advance();
        if modifiers.expand1 && name.starts_with('_') {
            return Err(GrammarError::new(
                "Inlined rules (_rule) cannot use the ?rule modifier.",
            ));
        }
        let mut params = Vec::new();
        if self.eat(&Tok::LBrace) {
            loop {
                match self.peek() {
                    Some(Tok::Rule(p)) => {
                        params.push(p.clone());
                        self.advance();
                    }
                    _ => return Err(self.error("Expecting a template parameter name")),
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RBrace, "Expecting '}' after template parameters")?;
        }
        let priority = self.priority()?;
        self.colon()?;
        let body = self.expansions()?;
        self.expect_newline()?;
        Ok(RuleDef {
            name,
            params,
            priority,
            modifiers,
            body,
        })
    }

    fn term_def(&mut self) -> GrammarResult<TermDef> {
        let Some(Tok::Terminal(name)) = self.advance() else {
            unreachable!("term_def is entered on a terminal name")
        };
        if self.peek() == Some(&Tok::Dot) && matches!(self.peek_at(1), Some(Tok::Terminal(_))) {
            return Err(self.error("Terminal names cannot contain dots"));
        }
        let priority = self.priority()?.unwrap_or(0);
        self.colon()?;
        let body = self.expansions()?;
        self.expect_newline()?;
        Ok(TermDef {
            name,
            priority,
            body,
        })
    }

    fn priority(&mut self) -> GrammarResult<Option<i64>> {
        if !self.eat(&Tok::Dot) {
            return Ok(None);
        }
        match self.advance() {
            Some(Tok::Number(n)) => Ok(Some(n)),
            _ => Err(self.error("Expecting a priority number after '.'")),
        }
    }

    fn colon(&mut self) -> GrammarResult<()> {
        match self.peek() {
            Some(Tok::Colon) => {
                self.advance();
                Ok(())
            }
            _ => Err(self.error("Expecting rule or terminal definition (missing colon)")),
        }
    }

    fn starts_atom(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Tok::LPar
                    | Tok::LBra
                    | Tok::Rule(_)
                    | Tok::Terminal(_)
                    | Tok::Str { .. }
                    | Tok::Regexp { .. }
            )
        )
    }

    /// `expansions: alias (("|" | NL_OR) alias)*`
    fn expansions(&mut self) -> GrammarResult<Expr> {
        let mut alternatives = Vec::new();
        alternatives.push(self.alternative()?);
        while self.eat(&Tok::Or) || self.eat(&Tok::NlOr) {
            alternatives.push(self.alternative()?);
        }
        Ok(Expr::Alternatives(alternatives))
    }

    /// `alias: expansion ("->" RULE)?`
    fn alternative(&mut self) -> GrammarResult<Alternative> {
        let mut items = Vec::new();
        while self.starts_atom() {
            items.push(self.expr()?);
        }
        let alias = if self.eat(&Tok::To) {
            match self.peek() {
                Some(Tok::Rule(name)) => {
                    let name = name.clone();
                    self.advance();
                    Some(name)
                }
                _ => return Err(self.error("Alias expects lowercase name")),
            }
        } else {
            None
        };
        match self.peek() {
            Some(Tok::Op(_)) => return Err(self.error("Misplaced operator")),
            Some(Tok::Tilde) => return Err(self.error("Misplaced operator")),
            Some(Tok::Number(_) | Tok::LBrace | Tok::Comma | Tok::DotDot) => {
                return Err(self.error("Incorrect type of value"));
            }
            _ => {}
        }
        Ok(Alternative { items, alias })
    }

    /// The count after `~` or `..` (`after`).
    fn repeat_count(&mut self, after: &str) -> GrammarResult<u32> {
        let pos = self.pos();
        match self.advance() {
            Some(Tok::Number(n)) if (0..=REPEAT_LIMIT as i64).contains(&n) => Ok(n as u32),
            Some(Tok::Number(n)) => Err(self.error_at(
                pos,
                &format!("Bad Range for repetition ({n} isn't allowed)"),
            )),
            _ => Err(self.error_at(pos, &format!("Expecting a number after '{after}'"))),
        }
    }

    /// `expr: atom (OP | "~" NUMBER (".." NUMBER)?)?`
    fn expr(&mut self) -> GrammarResult<Expr> {
        let atom = self.atom()?;
        match self.peek() {
            Some(Tok::Op(op)) => {
                let (min, max) = match op {
                    '?' => (0, Some(1)),
                    '*' => (0, None),
                    _ => (1, None),
                };
                self.advance();
                Ok(Expr::Repeat {
                    inner: Box::new(atom),
                    min,
                    max,
                })
            }
            Some(Tok::Tilde) => {
                self.advance();
                let pos = self.pos();
                let min = self.repeat_count("~")?;
                let max = if self.eat(&Tok::DotDot) {
                    let max = self.repeat_count("..")?;
                    if max < min {
                        return Err(self.error_at(
                            pos,
                            &format!(
                                "Bad Range for {} ({min}..{max} isn't allowed)",
                                atom_text(&atom)
                            ),
                        ));
                    }
                    max
                } else {
                    min
                };
                Ok(Expr::Repeat {
                    inner: Box::new(atom),
                    min,
                    max: Some(max),
                })
            }
            _ => Ok(atom),
        }
    }

    /// `atom: "(" expansions ")" | "[" expansions "]" | value`
    fn atom(&mut self) -> GrammarResult<Expr> {
        let pos = self.pos();
        match self.advance() {
            Some(Tok::LPar) => {
                let inner = self.group(pos)?;
                match self.peek() {
                    Some(Tok::RPar) => {
                        self.advance();
                        Ok(inner)
                    }
                    Some(Tok::RBra) => Err(self.error("Unmatched closing parenthesis")),
                    _ => Err(self.error_at(pos, "Unclosed parenthesis")),
                }
            }
            Some(Tok::LBra) => {
                let inner = self.group(pos)?;
                match self.peek() {
                    Some(Tok::RBra) => {
                        self.advance();
                        Ok(Expr::Repeat {
                            inner: Box::new(inner),
                            min: 0,
                            max: Some(1),
                        })
                    }
                    Some(Tok::RPar) => Err(self.error("Unmatched closing parenthesis")),
                    _ => Err(self.error_at(pos, "Unclosed parenthesis")),
                }
            }
            Some(Tok::Terminal(name)) => Ok(Expr::Symbol(Symbol::Terminal(name))),
            Some(Tok::Rule(name)) => {
                if self.eat(&Tok::LBrace) {
                    self.enter(pos)?;
                    let mut args = Vec::new();
                    args.push(self.value()?);
                    while self.eat(&Tok::Comma) {
                        args.push(self.value()?);
                    }
                    self.depth -= 1;
                    self.expect(&Tok::RBrace, "Expecting '}' after template arguments")?;
                    Ok(Expr::Template { name, args })
                } else {
                    Ok(Expr::Symbol(Symbol::Rule(name)))
                }
            }
            Some(Tok::Str { body, flags }) => {
                if self.eat(&Tok::DotDot) {
                    match self.advance() {
                        Some(Tok::Str { body: end, .. }) => {
                            Ok(Expr::Literal(range_literal(&body, &end)?))
                        }
                        _ => Err(self.error("Expecting a string literal after '..'")),
                    }
                } else {
                    Ok(Expr::Literal(string_literal(&body, &flags)?))
                }
            }
            Some(Tok::Regexp { body, flags }) => Ok(Expr::Literal(regex_literal(&body, &flags)?)),
            _ => unreachable!("atom is entered only on a token that starts one"),
        }
    }

    /// The expansions inside a parenthesis or bracket opened at `pos`,
    /// one level deeper.
    fn group(&mut self, pos: Pos) -> GrammarResult<Expr> {
        self.enter(pos)?;
        let inner = self.expansions();
        self.depth -= 1;
        inner
    }

    /// Go one level deeper, at the group or template arguments opened at
    /// `pos`, unless the grammar is already at [`MAX_NESTING`].
    fn enter(&mut self, pos: Pos) -> GrammarResult<()> {
        if self.depth == MAX_NESTING {
            return Err(self.error_at(
                pos,
                &format!(
                    "Grammar nests more than {MAX_NESTING} levels of parentheses or template arguments"
                ),
            ));
        }
        self.depth += 1;
        Ok(())
    }

    /// `value: terminal | nonterminal | literal | range | template_usage`
    /// (a template argument).
    fn value(&mut self) -> GrammarResult<Expr> {
        match self.peek() {
            Some(Tok::Rule(_) | Tok::Terminal(_) | Tok::Str { .. } | Tok::Regexp { .. }) => {
                self.atom()
            }
            _ => Err(self.error("Expecting a value")),
        }
    }
}

/// Parse grammar text into its statements.
pub fn parse_grammar(text: &str) -> GrammarResult<Vec<Statement>> {
    let tokens = tokenize(text)?;
    let mut parser = Parser {
        tokens,
        i: 0,
        text,
        depth: 0,
    };
    parser.parse_grammar()
}

/// How an atom is named in a repeat-range error: a symbol or template by
/// its name, a literal as it is written.
fn atom_text(atom: &Expr) -> String {
    match atom {
        Expr::Symbol(symbol) => symbol.name().to_string(),
        Expr::Literal(Pattern {
            kind: PatternKind::Str,
            value,
            ..
        }) => format!("\"{value}\""),
        Expr::Literal(Pattern {
            kind: PatternKind::Re,
            value,
            ..
        }) => format!("/{value}/"),
        Expr::Template { name, .. } => name.clone(),
        Expr::Alternatives(_) | Expr::Repeat { .. } => "group".to_string(),
    }
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/lark/parser_tests.rs"]
mod tests;
