//! Build scripts that provably do nothing but print: a script whose whole source is of a few
//! small forms is read, and what a run of it would print is worked out from the environment
//! it is given, with nothing compiled or run. The idea is Relax's
//! (makepad `libs/relax/front/src/unitgraph_noop.rs`): no list of packages, only forms checked
//! token by token; anything else is compiled and run as before.
//!
//! The forms ([`of`]): items `fn main() { .. }` and `use std::env;`, and in `main` only
//! `println!`, `print!`, `eprintln!` and `eprint!` of a string literal with no arguments,
//! `let NAME = <text>;`, and `if`/`else if`/`else` over conditions made of
//! `std::env::var(K).is_ok()`/`.is_err()`, `std::env::var_os(K).is_some()`/`.is_none()`,
//! `<text>.contains("..")`, `<text>.is_empty()`, `<text> == ".."`, `!=`, `!`, `&&`, `||` and
//! parentheses, where `<text>` is `std::env::var(K).unwrap_or_default()`, a `let` of one, or
//! a string literal. Every one of those compiles whatever the environment, cannot panic and
//! reads nothing but the environment, so the script's run is a function of the environment
//! alone: its output, empty stderr but for `eprint`, nothing in `OUT_DIR`, success.
//!
//! What is refused, and why: any attribute (`#![deny(..)]`, `#[cfg]`, a `#[path]`), since one
//! can change what compiles or which `main` there is; any macro but the four, since one can
//! define or import items (a `println` of its own); `mod`, `extern`, other `use`s and other
//! items, which bring in code or names; doc comments where rustc refuses them; raw
//! identifiers; and a format string with `{` or `}` but `{{` and `}}`. Whether the lints
//! rustc is given can fail the build is [`args`]'s to say.

use std::ops::Range;

use parser::{Edition, LexedStr, SyntaxKind, T};

/// A build script whose run is a function of its environment alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inert {
    main: Vec<Stmt>,
}

/// What a run of an [`Inert`] script writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Printed {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Stmt {
    Print { stderr: bool, text: String },
    If { arms: Vec<(Cond, Vec<Stmt>)>, otherwise: Vec<Stmt> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Cond {
    Not(Box<Cond>),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    /// `var(K).is_ok()`, `var_os(K).is_some()`.
    Set(String),
    Contains(Text, String),
    Equals(Text, Text),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Text {
    /// `std::env::var(K).unwrap_or_default()`: the value, or empty when it is not set.
    Var(String),
    Literal(String),
}

/// What a script is, when it is not inert: the first form that is not one of [`of`]'s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotInert(pub String);

impl std::fmt::Display for NotInert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The script of `source`, compiled for `edition`, as an [`Inert`]; else why it is not one.
pub fn of(source: &str, edition: Edition) -> Result<Inert, NotInert> {
    // rustc reads a source with its byte-order mark taken off and `\r\n` as `\n`; a `\r` left
    // is refused in a literal or a doc comment, and nothing here needs one.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source).replace("\r\n", "\n");
    if source.contains('\r') {
        return Err(not("a carriage return"));
    }
    let lexed = LexedStr::new(edition, &source);
    if let Some((_, error)) = lexed.errors().next() {
        return Err(NotInert(format!("a token rustc refuses ({error})")));
    }
    let tokens = Tokens::of(&lexed)?;
    let mut parser = Parser { tokens: &tokens, at: 0, env_imported: false, scopes: Vec::new() };
    parser.file()
}

fn not(what: &str) -> NotInert {
    NotInert(format!("it has {what}"))
}

impl Inert {
    /// What a run with the environment `var` would write: `var(name)` is the value of the
    /// variable `name` it is given, `None` when it is not.
    pub fn run<'a>(&self, var: impl Fn(&str) -> Option<&'a str>) -> Printed {
        let mut printed = Printed::default();
        run(&self.main, &var, &mut printed);
        printed
    }
}

fn run<'a>(stmts: &[Stmt], var: &impl Fn(&str) -> Option<&'a str>, printed: &mut Printed) {
    for stmt in stmts {
        match stmt {
            Stmt::Print { stderr, text } => {
                let to = if *stderr { &mut printed.stderr } else { &mut printed.stdout };
                to.extend_from_slice(text.as_bytes());
            }
            Stmt::If { arms, otherwise } => {
                let taken = arms.iter().find(|(cond, _)| holds(cond, var));
                run(taken.map_or(otherwise, |(_, then)| then), var, printed);
            }
        }
    }
}

fn holds<'a>(cond: &Cond, var: &impl Fn(&str) -> Option<&'a str>) -> bool {
    let text = |text: &Text| match text {
        Text::Var(name) => var(name).unwrap_or_default().to_owned(),
        Text::Literal(it) => it.clone(),
    };
    match cond {
        Cond::Not(it) => !holds(it, var),
        Cond::And(a, b) => holds(a, var) && holds(b, var),
        Cond::Or(a, b) => holds(a, var) || holds(b, var),
        Cond::Set(name) => var(name).is_some(),
        Cond::Contains(it, needle) => text(it).contains(needle.as_str()),
        Cond::Equals(a, b) => text(a) == text(b),
    }
}

/// Whether rustc run with `args` for the script compiles every [`Inert`] one: no lint it is
/// told to deny can fail it unless lints are capped, nothing is linked in that could be
/// missing, it is given no crate named `std` and no unstable option. `Err` names the
/// argument. What cargo passes for a script and the flags of `RUSTFLAGS` and `[lints]` are
/// let through; any other argument is refused.
pub fn args(args: &[String]) -> Result<(), NotInert> {
    let refused = |arg: &str| Err(NotInert(format!("rustc is given `{arg}`")));
    let capped = args.windows(2).any(|it| it[0] == "--cap-lints" && it[1] != "deny")
        || args.iter().any(|it| it.strip_prefix("--cap-lints=").is_some_and(|it| it != "deny"));
    // Flags whose value is the next argument, unless joined to it.
    const VALUED: [&str; 18] = [
        "--crate-name",
        "--edition",
        "--crate-type",
        "--emit",
        "--out-dir",
        "--error-format",
        "--json",
        "--diagnostic-width",
        "--color",
        "--check-cfg",
        "--cfg",
        "--cap-lints",
        "--target",
        "--remap-path-prefix",
        "--extern",
        "-L",
        "-C",
        "--codegen",
    ];
    const LINTS: [&str; 8] = ["-A", "--allow", "-W", "--warn", "--force-warn", "-D", "--deny", "-F"];
    let mut at = 0;
    while at < args.len() {
        let arg = args[at].as_str();
        at += 1;
        let (flag, value) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (arg, None),
        };
        if VALUED.contains(&flag) || LINTS.contains(&flag) || ["--forbid"].contains(&flag) {
            let value = match value {
                Some(value) => value,
                None => {
                    at += 1;
                    match args.get(at - 1) {
                        Some(value) => value.as_str(),
                        None => return refused(arg),
                    }
                }
            };
            match flag {
                "-D" | "--deny" | "-F" | "--forbid" if !capped && !tool_lint(value) => {
                    return refused(arg);
                }
                "--extern" if matches!(value.split('=').next(), Some("std" | "core" | "alloc")) => {
                    return refused(arg);
                }
                "-C" | "--codegen" => codegen(value).or_else(|()| refused(arg))?,
                "--crate-type" if value != "bin" => return refused(arg),
                _ => {}
            }
            continue;
        }
        if let Some(lint) = ["-A", "-W", "-D", "-F"].iter().find(|it| arg.starts_with(**it)) {
            if matches!(*lint, "-D" | "-F") && !capped && !tool_lint(&arg[2..]) {
                return refused(arg);
            }
            continue;
        }
        if let Some(option) = arg.strip_prefix("-C") {
            codegen(option).or_else(|()| refused(arg))?;
            continue;
        }
        if arg.starts_with("-L") || (!arg.starts_with('-') && arg.ends_with(".rs")) {
            continue;
        }
        return refused(arg);
    }
    Ok(())
}

/// Whether a lint is one of another tool's (`clippy::`, `rustdoc::`), which rustc does not run.
fn tool_lint(name: &str) -> bool {
    name.split_once("::").is_some_and(|(tool, _)| tool != "rust")
}

/// Whether `-C option` changes nothing of whether a script compiles: what cargo passes for a
/// profile and its own metadata, a target CPU or feature (one rustc does not know it warns of),
/// and the linker and its arguments, which blaze's own build of a script does not use: it
/// links with `cc` whatever rustc was told.
fn codegen(option: &str) -> Result<(), ()> {
    let name = option.split_once('=').map_or(option, |it| it.0);
    const HARMLESS: [&str; 19] = [
        "target-cpu",
        "target-feature",
        "linker",
        "link-arg",
        "link-args",
        "opt-level",
        "debuginfo",
        "debug-assertions",
        "overflow-checks",
        "codegen-units",
        "incremental",
        "metadata",
        "extra-filename",
        "embed-bitcode",
        "strip",
        "split-debuginfo",
        "panic",
        "force-frame-pointers",
        "lto",
    ];
    if HARMLESS.contains(&name) { Ok(()) } else { Err(()) }
}

/// The tokens of a source but whitespace and comments, each with whether it follows the one
/// before with nothing between (`::` is two joined `:`).
struct Tokens<'a> {
    kinds: Vec<SyntaxKind>,
    texts: Vec<&'a str>,
    joined: Vec<bool>,
    /// Where an outer doc comment was before the token: rustc wants an item after one.
    outer_doc: Vec<bool>,
}

impl<'a> Tokens<'a> {
    fn of(lexed: &'a LexedStr<'a>) -> Result<Tokens<'a>, NotInert> {
        let mut tokens =
            Tokens { kinds: Vec::new(), texts: Vec::new(), joined: Vec::new(), outer_doc: Vec::new() };
        let mut joined = false;
        let mut outer_doc = false;
        for at in 0..lexed.len() {
            let (kind, text) = (lexed.kind(at), lexed.text(at));
            match kind {
                SyntaxKind::WHITESPACE => joined = false,
                SyntaxKind::COMMENT => {
                    joined = false;
                    match doc(text) {
                        Some(Doc::Inner) if !tokens.kinds.is_empty() || outer_doc => {
                            return Err(not("an inner doc comment after an item"));
                        }
                        Some(Doc::Outer) => outer_doc = true,
                        _ => {}
                    }
                }
                SyntaxKind::SHEBANG | SyntaxKind::FRONTMATTER => return Err(not("a shebang")),
                _ => {
                    if kind == SyntaxKind::IDENT && text.starts_with("r#") {
                        return Err(not("a raw identifier"));
                    }
                    tokens.kinds.push(kind);
                    tokens.texts.push(text);
                    tokens.joined.push(joined);
                    tokens.outer_doc.push(outer_doc);
                    joined = true;
                    outer_doc = false;
                }
            }
        }
        if outer_doc {
            return Err(not("a doc comment that documents nothing"));
        }
        Ok(tokens)
    }
}

enum Doc {
    Inner,
    Outer,
}

/// Which doc comment `comment` is, as rustc's lexer tells them.
fn doc(comment: &str) -> Option<Doc> {
    if let Some(rest) = comment.strip_prefix("//") {
        return match rest.as_bytes() {
            [b'!', ..] => Some(Doc::Inner),
            [b'/', b'/', ..] => None,
            [b'/', ..] => Some(Doc::Outer),
            _ => None,
        };
    }
    let rest = comment.strip_prefix("/*")?;
    match rest.as_bytes() {
        [b'!', ..] => Some(Doc::Inner),
        [b'*', b'*', ..] | [b'*', b'/'] => None,
        [b'*', ..] => Some(Doc::Outer),
        _ => None,
    }
}

/// Values of expressions as rustc types them.
enum Value {
    Bool(Cond),
    /// A `String` or a `&str`: either compares with the other and has `contains`.
    Text(Text),
}

struct Parser<'t, 'a> {
    tokens: &'t Tokens<'a>,
    at: usize,
    env_imported: bool,
    /// The `let`s in scope, innermost last: each name with the variable it holds the value of.
    scopes: Vec<(String, Text)>,
}

impl Parser<'_, '_> {
    fn peek(&self, ahead: usize) -> Option<(SyntaxKind, &str)> {
        let at = self.at + ahead;
        Some((*self.tokens.kinds.get(at)?, self.tokens.texts[at]))
    }

    fn is(&self, kind: SyntaxKind) -> bool {
        self.peek(0).is_some_and(|it| it.0 == kind)
    }

    fn is_ident(&self, name: &str) -> bool {
        self.peek(0).is_some_and(|it| it.0 == SyntaxKind::IDENT && it.1 == name)
    }

    fn here(&self) -> String {
        match self.peek(0) {
            Some((_, text)) => format!("`{text}`"),
            None => "the end".to_owned(),
        }
    }

    fn unexpected<T>(&self, wanted: &str) -> Result<T, NotInert> {
        Err(NotInert(format!("{} where {wanted} is", self.here())))
    }

    fn eat(&mut self, kind: SyntaxKind) -> bool {
        let found = self.is(kind);
        self.at += usize::from(found);
        found
    }

    fn expect(&mut self, kind: SyntaxKind, wanted: &str) -> Result<(), NotInert> {
        if self.eat(kind) { Ok(()) } else { self.unexpected(wanted) }
    }

    fn expect_ident(&mut self, name: &str) -> Result<(), NotInert> {
        if self.is_ident(name) {
            self.at += 1;
            Ok(())
        } else {
            self.unexpected(&format!("`{name}`"))
        }
    }

    /// Two tokens of `kind` with nothing between: `::`, `&&`, `||`.
    fn eat_joined(&mut self, first: SyntaxKind, second: SyntaxKind) -> bool {
        let found = self.is(first)
            && self.tokens.kinds.get(self.at + 1) == Some(&second)
            && self.tokens.joined[self.at + 1];
        self.at += 2 * usize::from(found);
        found
    }

    fn eat_path_sep(&mut self) -> bool {
        self.eat_joined(T![:], T![:])
    }

    fn expect_path_sep(&mut self) -> Result<(), NotInert> {
        if self.eat_path_sep() { Ok(()) } else { self.unexpected("`::`") }
    }

    fn file(&mut self) -> Result<Inert, NotInert> {
        let mut main = None;
        // rustc wants an item after an outer doc comment.
        let documented = self.tokens.outer_doc.iter().enumerate().filter(|it| *it.1);
        let documented: Vec<usize> = documented.map(|it| it.0).collect();
        let mut documented_items = 0;
        while self.at < self.tokens.kinds.len() {
            if documented.contains(&self.at) {
                documented_items += 1;
            }
            if self.is(T![use]) {
                self.at += 1;
                self.eat_path_sep();
                self.expect_ident("std")?;
                self.expect_path_sep()?;
                self.expect_ident("env")?;
                self.expect(T![;], "`;`")?;
                if std::mem::replace(&mut self.env_imported, true) {
                    return Err(not("`env` imported twice"));
                }
            } else if self.is(T![fn]) {
                self.at += 1;
                self.expect_ident("main")?;
                self.expect(T!['('], "`(`")?;
                self.expect(T![')'], "`)`")?;
                if main.is_some() {
                    return Err(not("two `main`s"));
                }
                main = Some(self.at);
                self.skip_block()?;
            } else if self.is(T![#]) {
                return Err(not("an attribute"));
            } else {
                return self.unexpected("`fn main` or `use std::env;`");
            }
        }
        if documented_items != documented.len() {
            return Err(not("a doc comment on something other than an item"));
        }
        // `main` is read once every `use` is known: an item's place in the file does not matter.
        let Some(main) = main else { return Err(not("no `main`")) };
        self.at = main;
        let stmts = self.block()?;
        Ok(Inert { main: stmts })
    }

    /// Past a block whose braces match, without reading it.
    fn skip_block(&mut self) -> Result<(), NotInert> {
        if !self.is(T!['{']) {
            return self.unexpected("`{`");
        }
        let mut depth = 0usize;
        while let Some((kind, _)) = self.peek(0) {
            self.at += 1;
            match kind {
                T!['{'] => depth += 1,
                T!['}'] => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        Err(not("a block that does not end"))
    }

    fn block(&mut self) -> Result<Vec<Stmt>, NotInert> {
        self.expect(T!['{'], "`{`")?;
        let scope = self.scopes.len();
        let mut stmts = Vec::new();
        while !self.eat(T!['}']) {
            if self.is(T![if]) {
                stmts.push(self.if_stmt()?);
            } else if self.is(T![let]) {
                self.let_stmt()?;
            } else {
                stmts.push(self.print()?);
                // A block's last expression needs no `;`.
                if !self.is(T!['}']) {
                    self.expect(T![;], "`;`")?;
                }
            }
        }
        self.scopes.truncate(scope);
        Ok(stmts)
    }

    fn let_stmt(&mut self) -> Result<(), NotInert> {
        self.expect(T![let], "`let`")?;
        let Some((SyntaxKind::IDENT, name)) = self.peek(0) else {
            return self.unexpected("a name");
        };
        let name = name.to_owned();
        self.at += 1;
        self.expect(T![=], "`=`")?;
        let Value::Text(text) = self.unary()? else {
            return Err(not("a `let` of something other than text"));
        };
        if matches!(self.peek(0), Some((T![.], _))) {
            return self.unexpected("`;`");
        }
        self.expect(T![;], "`;`")?;
        self.scopes.push((name, text));
        Ok(())
    }

    fn print(&mut self) -> Result<Stmt, NotInert> {
        let Some((SyntaxKind::IDENT, name)) = self.peek(0) else {
            return self.unexpected("`println!`");
        };
        let (stderr, line) = match name {
            "println" => (false, true),
            "print" => (false, false),
            "eprintln" => (true, true),
            "eprint" => (true, false),
            _ => return self.unexpected("`println!`"),
        };
        self.at += 1;
        self.expect(T![!], "`!`")?;
        self.expect(T!['('], "`(`")?;
        let mut text = String::new();
        if self.is(SyntaxKind::STRING) {
            text = format_string(self.string()?)?;
            self.eat(T![,]);
        } else if !line {
            // `print!()` needs a format string.
            return self.unexpected("a format string");
        }
        self.expect(T![')'], "`)`")?;
        if line {
            text.push('\n');
        }
        Ok(Stmt::Print { stderr, text })
    }

    fn string(&mut self) -> Result<String, NotInert> {
        let Some((SyntaxKind::STRING, text)) = self.peek(0) else {
            return self.unexpected("a string");
        };
        self.at += 1;
        literal(text)
    }

    fn if_stmt(&mut self) -> Result<Stmt, NotInert> {
        let mut arms = Vec::new();
        let mut otherwise = Vec::new();
        loop {
            self.expect(T![if], "`if`")?;
            let cond = self.bool()?;
            arms.push((cond, self.block()?));
            if !self.eat(T![else]) {
                break;
            }
            if !self.is(T![if]) {
                otherwise = self.block()?;
                break;
            }
        }
        Ok(Stmt::If { arms, otherwise })
    }

    fn bool(&mut self) -> Result<Cond, NotInert> {
        match self.or()? {
            Value::Bool(cond) => Ok(cond),
            Value::Text(_) => Err(not("text where a condition is")),
        }
    }

    fn or(&mut self) -> Result<Value, NotInert> {
        let mut value = self.and()?;
        while self.eat_joined(T![|], T![|]) {
            let rhs = self.and()?;
            value = Value::Bool(Cond::Or(Box::new(as_bool(value)?), Box::new(as_bool(rhs)?)));
        }
        Ok(value)
    }

    fn and(&mut self) -> Result<Value, NotInert> {
        let mut value = self.compare()?;
        while self.eat_joined(T![&], T![&]) {
            let rhs = self.compare()?;
            value = Value::Bool(Cond::And(Box::new(as_bool(value)?), Box::new(as_bool(rhs)?)));
        }
        Ok(value)
    }

    fn compare(&mut self) -> Result<Value, NotInert> {
        let lhs = self.unary()?;
        let equal = if self.eat_joined(T![=], T![=]) {
            true
        } else if self.eat_joined(T![!], T![=]) {
            false
        } else {
            return Ok(lhs);
        };
        let rhs = self.unary()?;
        let (Value::Text(lhs), Value::Text(rhs)) = (lhs, rhs) else {
            return Err(not("a comparison of something other than text"));
        };
        let cond = Cond::Equals(lhs, rhs);
        Ok(Value::Bool(if equal { cond } else { Cond::Not(Box::new(cond)) }))
    }

    fn unary(&mut self) -> Result<Value, NotInert> {
        if self.eat(T![!]) {
            return Ok(Value::Bool(Cond::Not(Box::new(as_bool(self.unary()?)?))));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Value, NotInert> {
        let mut value = self.primary()?;
        while self.eat(T![.]) {
            let Some((SyntaxKind::IDENT, method)) = self.peek(0) else {
                return self.unexpected("a method");
            };
            let method = method.to_owned();
            self.at += 1;
            self.expect(T!['('], "`(`")?;
            value = match (value, method.as_str()) {
                (Value::Text(text), "contains") => {
                    let needle = self.string()?;
                    Value::Bool(Cond::Contains(text, needle))
                }
                (Value::Text(text), "is_empty") => {
                    Value::Bool(Cond::Equals(text, Text::Literal(String::new())))
                }
                _ => return Err(NotInert(format!("`.{method}()` where it is not known"))),
            };
            self.expect(T![')'], "`)`")?;
        }
        Ok(value)
    }

    fn primary(&mut self) -> Result<Value, NotInert> {
        if self.eat(T!['(']) {
            let value = self.or()?;
            self.expect(T![')'], "`)`")?;
            return Ok(value);
        }
        if self.is(SyntaxKind::STRING) {
            return Ok(Value::Text(Text::Literal(self.string()?)));
        }
        let local = match self.peek(0) {
            Some((SyntaxKind::IDENT, name)) if self.tokens.kinds.get(self.at + 1) != Some(&T![:]) => {
                self.scopes.iter().rev().find(|it| it.0 == name).map(|it| it.1.clone())
            }
            _ => None,
        };
        if let Some(text) = local {
            self.at += 1;
            return Ok(Value::Text(text));
        }
        self.env_call()
    }

    /// `std::env::var(K)` or `var_os(K)`, and what is asked of it.
    fn env_call(&mut self) -> Result<Value, NotInert> {
        if self.eat_path_sep() {
            self.expect_ident("std")?;
            self.expect_path_sep()?;
        } else if self.is_ident("std") {
            self.at += 1;
            self.expect_path_sep()?;
        } else if !self.env_imported {
            return self.unexpected("`std::env::var`");
        }
        self.expect_ident("env")?;
        self.expect_path_sep()?;
        let os = if self.is_ident("var") {
            false
        } else if self.is_ident("var_os") {
            true
        } else {
            return self.unexpected("`var` or `var_os`");
        };
        self.at += 1;
        self.expect(T!['('], "`(`")?;
        let name = self.string()?;
        self.expect(T![')'], "`)`")?;
        // `getenv` of a name with `=` finds the variable named by what is before it.
        if name.is_empty() || name.contains(['=', '\0']) {
            return Err(NotInert(format!("a variable named {name:?}")));
        }
        self.expect(T![.], "`.`")?;
        let Some((SyntaxKind::IDENT, method)) = self.peek(0) else {
            return self.unexpected("a method");
        };
        let value = match (os, method) {
            (false, "is_ok") | (true, "is_some") => Value::Bool(Cond::Set(name)),
            (false, "is_err") | (true, "is_none") => {
                Value::Bool(Cond::Not(Box::new(Cond::Set(name))))
            }
            (false, "unwrap_or_default") => Value::Text(Text::Var(name)),
            _ => return Err(NotInert(format!("`.{method}()` of a variable"))),
        };
        self.at += 1;
        self.expect(T!['('], "`(`")?;
        self.expect(T![')'], "`)`")?;
        Ok(value)
    }
}

fn as_bool(value: Value) -> Result<Cond, NotInert> {
    match value {
        Value::Bool(cond) => Ok(cond),
        Value::Text(_) => Err(not("text where a condition is")),
    }
}

/// The value of the string literal `text`, `"..."` or `r#"..."#`, as rustc reads it.
fn literal(text: &str) -> Result<String, NotInert> {
    let refused = || Err(NotInert(format!("the literal {text}")));
    if let Some(raw) = text.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let fence = "#".repeat(hashes);
        let Some(body) = raw[hashes..].strip_prefix('"').and_then(|it| it.strip_suffix(&format!("\"{fence}")))
        else {
            return refused();
        };
        return Ok(body.to_owned());
    }
    let Some(body) = text.strip_prefix('"').and_then(|it| it.strip_suffix('"')) else {
        return refused();
    };
    let mut value = String::new();
    let mut failed = false;
    rustc_literal_escaper::unescape_str(body, |_: Range<usize>, it| match it {
        Ok(c) => value.push(c),
        Err(_) => failed = true,
    });
    if failed { refused() } else { Ok(value) }
}

/// What `format_args!` makes of `value` with no arguments: `{{` and `}}` are a brace; any
/// other brace would take an argument, or capture a variable.
fn format_string(value: String) -> Result<String, NotInert> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' || c == '}' {
            if chars.next_if_eq(&c).is_none() {
                return Err(not("a format string that takes an argument"));
            }
        }
        out.push(c);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
