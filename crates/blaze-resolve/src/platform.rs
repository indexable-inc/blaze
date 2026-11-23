//! The platform of a `[target.<platform>.dependencies]` table: a target triple
//! (`x86_64-unknown-linux-gnu`) or a cfg expression (`cfg(any(unix, target_os = "wasi"))`),
//! and whether it holds for a target's name and cfg values. The grammar, the names and the
//! shapes are those of cargo-platform 0.3.3 (MIT or Apache-2.0), which cargo 1.99.0
//! (5f94df4789) parses these keys with, so a caller moves by name; the code is blaze's.
//!
//! The grammar:
//!
//! ```text
//! expr  = "all" "(" list ")" | "any" "(" list ")" | "not" "(" expr ")" | cfg
//! list  = [ expr { "," expr } [ "," ] ]
//! cfg   = ident [ "=" string ]
//! ident = [ "r#" ] ( "_" | ASCII letter ) { "_" | ASCII letter | ASCII digit }
//! ```
//!
//! with spaces (only ` `) between tokens and strings in `"` without escapes. `true` and `false`
//! as a bare name are [`CfgExpr::True`] and [`CfgExpr::False`]. Every refusal is a [`CfgError`]
//! naming the text and the byte offset in it.

use std::alloc::Global;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::str::FromStr;

/// What a `[target.<platform>]` key names.
#[derive(Eq, PartialEq, Hash, Ord, PartialOrd, Clone, Debug)]
pub enum Platform {
    /// A target triple, like `x86_64-apple-darwin`.
    Name(String),
    /// A cfg expression, like `cfg(windows)`.
    Cfg(CfgExpr),
}

/// A cfg expression.
#[derive(Eq, PartialEq, Hash, Ord, PartialOrd, Clone, Debug)]
pub enum CfgExpr {
    Not(Box<CfgExpr>),
    All(Vec<CfgExpr>),
    Any(Vec<CfgExpr>),
    Value(Cfg),
    True,
    False,
}

/// One cfg value of a target.
#[derive(Eq, PartialEq, Hash, Ord, PartialOrd, Clone, Debug)]
pub enum Cfg {
    /// A name, like `unix`.
    Name(Ident),
    /// A key and its value, like `target_os = "linux"`.
    KeyPair(Ident, String),
}

/// An identifier of a cfg. `raw` (`r#async`) is how it was written: it is shown, and does not
/// take part in equality, order or hash (`foo` is `r#foo`).
#[derive(Clone, Debug)]
pub struct Ident {
    pub name: String,
    pub raw: bool,
}

/// The names a cfg should not use as identifiers: Rust's keywords that may stand there today
/// (cargo checks only these two; the others depend on the edition).
const KEYWORDS: [&str; 2] = ["true", "false"];

impl Ident {
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

impl PartialEq for Ident {
    fn eq(&self, other: &Ident) -> bool {
        self.name == other.name
    }
}

impl Eq for Ident {}

impl PartialEq<str> for Ident {
    fn eq(&self, other: &str) -> bool {
        self.name == other
    }
}

impl PartialEq<&str> for Ident {
    fn eq(&self, other: &&str) -> bool {
        self.name == *other
    }
}

impl Hash for Ident {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Ident) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// By name only, so the order agrees with equality.
impl Ord for Ident {
    fn cmp(&self, other: &Ident) -> Ordering {
        self.name.cmp(&other.name)
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.raw {
            f.write_str("r#")?;
        }
        f.write_str(&self.name)
    }
}

/// A text that is no platform: the text parsed (for `cfg(..)`, what is inside the parentheses,
/// as cargo names it), the byte offset in it, and what is wrong there.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CfgError {
    pub text: String,
    pub at: usize,
    pub what: CfgWhat,
}

/// What is wrong with a platform's text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CfgWhat {
    /// A `"` with no `"` after it.
    UnterminatedString,
    /// A character no token starts with.
    UnexpectedChar(char),
    /// One token where another was wanted.
    UnexpectedToken { expected: &'static str, found: &'static str },
    /// The text ended where `expected` was wanted.
    Incomplete { expected: &'static str },
    /// More after a whole expression: the rest from `at`.
    Trailing,
    /// A target name with a `(`, which only `cfg(..)` may have.
    TargetParen,
    /// A target name with a character a triple has not.
    TargetChar(char),
}

impl fmt::Display for CfgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to parse `{}` as a cfg expression: ", self.text)?;
        match self.what {
            CfgWhat::UnterminatedString => f.write_str("unterminated string in cfg")?,
            CfgWhat::UnexpectedChar(ch) => write!(
                f,
                "unexpected character `{ch}` in cfg, expected parens, a comma, an identifier, or \
                 a string"
            )?,
            CfgWhat::UnexpectedToken { expected, found } => {
                write!(f, "expected {expected}, found {found}")?
            }
            CfgWhat::Incomplete { expected } => {
                write!(f, "expected {expected}, but cfg expression ended")?
            }
            CfgWhat::Trailing => {
                let rest = self.text.get(self.at..).unwrap_or("");
                write!(f, "unexpected content `{rest}` found after cfg expression")?
            }
            CfgWhat::TargetParen => f.write_str(
                "invalid target specifier: unexpected `(` character, cfg expressions must start \
                 with `cfg(`",
            )?,
            CfgWhat::TargetChar(ch) => {
                write!(f, "invalid target specifier: unexpected character {ch} in target name")?
            }
        }
        write!(f, " (at byte {})", self.at)
    }
}

impl core::error::Error for CfgError {}

impl Platform {
    /// Whether the platform holds for the target `name` with the cfg values `cfg`.
    pub fn matches(&self, name: &str, cfg: &[Cfg]) -> bool {
        match self {
            Platform::Name(p) => p == name,
            Platform::Cfg(e) => e.matches(cfg),
        }
    }

    /// Warns of the cfg values a dependency's platform cannot select on: `test`,
    /// `debug_assertions`, `proc_macro` and `feature = ..` (cargo's words).
    pub fn check_cfg_attributes(&self, warnings: &mut Vec<String>) {
        if let Platform::Cfg(e) = self {
            e.walk(&mut |cfg| match cfg {
                Cfg::Name(name) => {
                    if matches!(name.as_str(), "test" | "debug_assertions" | "proc_macro") {
                        warnings.push(format!(
                            "Found `{name}` in `target.'cfg(...)'.dependencies`. This value is \
                             not supported for selecting dependencies and will not work as \
                             expected. To learn more visit \
                             https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#platform-specific-dependencies"
                        ));
                    }
                }
                Cfg::KeyPair(name, _) => {
                    if name.as_str() == "feature" {
                        warnings.push(
                            "Found `feature = ...` in `target.'cfg(...)'.dependencies`. This key \
                             is not supported for selecting dependencies and will not work as \
                             expected. Use the [features] section instead: \
                             https://doc.rust-lang.org/cargo/reference/features.html"
                                .to_owned(),
                        );
                    }
                }
            });
        }
    }

    /// Warns of a keyword written as a cfg's identifier without `r#` (`cfg(true = "x")`), which
    /// cargo accepted once and is phasing out; `path` is the file the platform is from.
    pub fn check_cfg_keywords(&self, warnings: &mut Vec<String>, path: &Path) {
        if let Platform::Cfg(e) = self {
            e.walk(&mut |cfg| {
                let (Cfg::Name(name) | Cfg::KeyPair(name, _)) = cfg;
                if !name.raw && KEYWORDS.contains(&name.as_str()) {
                    warnings.push(format!(
                        "[{}] future-incompatibility: `cfg({cfg})` is deprecated as `{name}` is \
                         a keyword and not an identifier and should not have have been accepted \
                         in this position.\n | this was previously accepted by Cargo but is \
                         being phased out; it will become a hard error in a future release!\n \
                         |\n | help: use raw-idents instead: `cfg(r#{name})`",
                        path.display()
                    ));
                }
            });
        }
    }
}

impl FromStr for Platform {
    type Err = CfgError;

    fn from_str(text: &str) -> Result<Platform, CfgError> {
        if let Some(inner) = text.strip_prefix("cfg(").and_then(|s| s.strip_suffix(')')) {
            return inner.parse().map(Platform::Cfg);
        }
        let bad = text.char_indices().find(|&(_, c)| {
            !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
        });
        match bad {
            None => Ok(Platform::Name(text.to_owned())),
            Some((at, ch)) => {
                let what = match text.contains('(') {
                    true => CfgWhat::TargetParen,
                    false => CfgWhat::TargetChar(ch),
                };
                Err(CfgError { text: text.to_owned(), at, what })
            }
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Platform::Name(n) => f.write_str(n),
            Platform::Cfg(e) => write!(f, "cfg({e})"),
        }
    }
}

/// As its text, as cargo writes it in `cargo metadata`.
impl serde::Serialize for Platform {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl CfgExpr {
    /// Whether `key` is `cfg(..)` around an expression that holds for `target_cfg`; false for
    /// anything else, an expression that does not parse included (cargo's `[target]` tables of
    /// a config, whose other keys are triples).
    pub fn matches_key(key: &str, target_cfg: &[Cfg]) -> bool {
        match key.strip_prefix("cfg(").and_then(|s| s.strip_suffix(')')) {
            Some(inner) => inner.parse::<CfgExpr>().is_ok_and(|e| e.matches(target_cfg)),
            None => false,
        }
    }

    pub fn matches(&self, cfg: &[Cfg]) -> bool {
        match self {
            CfgExpr::Not(e) => !e.matches(cfg),
            CfgExpr::All(es) => es.iter().all(|e| e.matches(cfg)),
            CfgExpr::Any(es) => es.iter().any(|e| e.matches(cfg)),
            CfgExpr::Value(v) => cfg.contains(v),
            CfgExpr::True => true,
            CfgExpr::False => false,
        }
    }

    /// Calls `f` on each value of the expression, in the order written.
    fn walk(&self, f: &mut impl FnMut(&Cfg)) {
        match self {
            CfgExpr::Not(e) => e.walk(f),
            CfgExpr::All(es) | CfgExpr::Any(es) => es.iter().for_each(|e| e.walk(f)),
            CfgExpr::Value(v) => f(v),
            CfgExpr::True | CfgExpr::False => {}
        }
    }
}

impl FromStr for CfgExpr {
    type Err = CfgError;

    fn from_str(text: &str) -> Result<CfgExpr, CfgError> {
        let mut p = Parser { text, at: 0 };
        let e = p.expr()?;
        p.end()?;
        Ok(e)
    }
}

impl FromStr for Cfg {
    type Err = CfgError;

    fn from_str(text: &str) -> Result<Cfg, CfgError> {
        let mut p = Parser { text, at: 0 };
        let c = p.cfg()?;
        p.end()?;
        Ok(c)
    }
}

impl fmt::Display for CfgExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |f: &mut fmt::Formatter<'_>, name: &str, es: &[CfgExpr]| {
            write!(f, "{name}(")?;
            for (i, e) in es.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{e}")?;
            }
            f.write_str(")")
        };
        match self {
            CfgExpr::Not(e) => write!(f, "not({e})"),
            CfgExpr::All(es) => list(f, "all", es),
            CfgExpr::Any(es) => list(f, "any", es),
            CfgExpr::Value(v) => write!(f, "{v}"),
            CfgExpr::True => f.write_str("true"),
            CfgExpr::False => f.write_str("false"),
        }
    }
}

impl fmt::Display for Cfg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cfg::Name(name) => write!(f, "{name}"),
            Cfg::KeyPair(key, value) => write!(f, "{key} = \"{value}\""),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Token<'t> {
    LeftParen,
    RightParen,
    Comma,
    Equals,
    Ident { raw: bool, name: &'t str },
    String(&'t str),
}

impl Token<'_> {
    fn describe(self) -> &'static str {
        match self {
            Token::LeftParen => "`(`",
            Token::RightParen => "`)`",
            Token::Comma => "`,`",
            Token::Equals => "`=`",
            Token::Ident { .. } => "an identifier",
            Token::String(_) => "a string",
        }
    }
}

fn ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn ident_rest(ch: char) -> bool {
    ident_start(ch) || ch.is_ascii_digit()
}

/// A cursor over a cfg expression's text.
struct Parser<'t> {
    text: &'t str,
    at: usize,
}

impl<'t> Parser<'t> {
    fn err(&self, at: usize, what: CfgWhat) -> CfgError {
        CfgError { text: self.text.to_owned(), at, what }
    }

    fn skip_spaces(&mut self) {
        while self.text.as_bytes().get(self.at) == Some(&b' ') {
            self.at += 1;
        }
    }

    /// The next token and where it starts, or `None` at the end.
    fn next(&mut self) -> Result<Option<(usize, Token<'t>)>, CfgError> {
        self.skip_spaces();
        let (text, start) = (self.text, self.at);
        let Some(ch) = text[start..].chars().next() else { return Ok(None) };
        self.at += ch.len_utf8();
        let token = match ch {
            '(' => Token::LeftParen,
            ')' => Token::RightParen,
            ',' => Token::Comma,
            '=' => Token::Equals,
            '"' => match text[self.at..].find('"') {
                Some(len) => {
                    let s = &text[self.at..self.at + len];
                    self.at += len + 1;
                    Token::String(s)
                }
                None => return Err(self.err(start, CfgWhat::UnterminatedString)),
            },
            'r' if text[self.at..].starts_with('#') => {
                self.at += 1;
                match text[self.at..].chars().next() {
                    Some(c) if ident_start(c) => Token::Ident { raw: true, name: self.ident() },
                    Some(c) => return Err(self.err(self.at, CfgWhat::UnexpectedChar(c))),
                    None => {
                        let expected = "identifier";
                        return Err(self.err(self.at, CfgWhat::Incomplete { expected }));
                    }
                }
            }
            c if ident_start(c) => {
                self.at = start;
                Token::Ident { raw: false, name: self.ident() }
            }
            c => return Err(self.err(start, CfgWhat::UnexpectedChar(c))),
        };
        Ok(Some((start, token)))
    }

    /// The identifier at the cursor, whose first character is an identifier's start.
    fn ident(&mut self) -> &'t str {
        let (text, start) = (self.text, self.at);
        let rest = &text[start..];
        let len = rest.find(|c: char| !ident_rest(c)).unwrap_or(rest.len());
        self.at += len;
        &text[start..start + len]
    }

    /// Takes `token` if it is next; leaves the cursor otherwise, an error included (the caller
    /// meets it again).
    fn take(&mut self, token: Token<'_>) -> bool {
        let save = self.at;
        match self.next() {
            Ok(Some((_, t))) if t == token => true,
            _ => {
                self.at = save;
                false
            }
        }
    }

    fn eat(&mut self, token: Token<'_>) -> Result<(), CfgError> {
        let end = self.text.len();
        match self.next()? {
            Some((_, t)) if t == token => Ok(()),
            Some((at, t)) => Err(self.err(
                at,
                CfgWhat::UnexpectedToken { expected: token.describe(), found: t.describe() },
            )),
            None => Err(self.err(end, CfgWhat::Incomplete { expected: token.describe() })),
        }
    }

    fn expr(&mut self) -> Result<CfgExpr, CfgError> {
        enum Frame {
            List(CfgExpr),
            Not,
        }

        let alloc = Global;
        let mut stack = blaze_own::Vec::new_in(alloc);

        'parse: loop {
            let save = self.at;
            let mut expr = match self.next()? {
                Some((_, Token::Ident { raw: false, name: op @ ("all" | "any") })) => {
                    let es = Vec::new_in(alloc);
                    self.eat(Token::LeftParen)?;
                    let list = if op == "all" { CfgExpr::All(es) } else { CfgExpr::Any(es) };
                    if self.take(Token::RightParen) {
                        list
                    } else {
                        stack.push(Frame::List(list));
                        continue 'parse;
                    }
                }
                Some((_, Token::Ident { raw: false, name: "not" })) => {
                    self.eat(Token::LeftParen)?;
                    stack.push(Frame::Not);
                    continue 'parse;
                }
                Some(_) => {
                    self.at = save;
                    match self.cfg()? {
                        Cfg::Name(n) if n == "true" => CfgExpr::True,
                        Cfg::Name(n) if n == "false" => CfgExpr::False,
                        v => CfgExpr::Value(v),
                    }
                }
                None => {
                    let expected = "start of a cfg expression";
                    return Err(self.err(self.text.len(), CfgWhat::Incomplete { expected }));
                }
            };

            while let Some(frame) = stack.pop() {
                match frame {
                    Frame::Not => {
                        self.eat(Token::RightParen)?;
                        expr = CfgExpr::Not(Box::new_in(expr, alloc));
                    }
                    Frame::List(mut list) => {
                        if let CfgExpr::All(es) | CfgExpr::Any(es) = &mut list {
                            es.push(expr);
                        }
                        if !self.take(Token::Comma) {
                            self.eat(Token::RightParen)?;
                            expr = list;
                        } else if self.take(Token::RightParen) {
                            expr = list;
                        } else {
                            stack.push(Frame::List(list));
                            continue 'parse;
                        }
                    }
                }
            }
            return Ok(expr);
        }
    }

    fn cfg(&mut self) -> Result<Cfg, CfgError> {
        let end = self.text.len();
        match self.next()? {
            Some((_, Token::Ident { raw, name })) => {
                let ident = Ident { name: name.to_owned(), raw };
                if !self.take(Token::Equals) {
                    return Ok(Cfg::Name(ident));
                }
                match self.next()? {
                    Some((_, Token::String(value))) => Ok(Cfg::KeyPair(ident, value.to_owned())),
                    Some((at, t)) => Err(self.err(
                        at,
                        CfgWhat::UnexpectedToken { expected: "a string", found: t.describe() },
                    )),
                    None => Err(self.err(end, CfgWhat::Incomplete { expected: "a string" })),
                }
            }
            Some((at, t)) => Err(self.err(
                at,
                CfgWhat::UnexpectedToken { expected: "identifier", found: t.describe() },
            )),
            None => Err(self.err(end, CfgWhat::Incomplete { expected: "identifier" })),
        }
    }

    /// Nothing but spaces may follow a whole expression.
    fn end(&mut self) -> Result<(), CfgError> {
        self.skip_spaces();
        match self.at == self.text.len() {
            true => Ok(()),
            false => Err(self.err(self.at, CfgWhat::Trailing)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> Cfg {
        Cfg::Name(Ident { name: n.to_owned(), raw: false })
    }

    fn pair(k: &str, v: &str) -> Cfg {
        Cfg::KeyPair(Ident { name: k.to_owned(), raw: false }, v.to_owned())
    }

    #[test]
    fn parses_and_shows_back() {
        for (text, shown) in [
            ("cfg(unix)", "cfg(unix)"),
            ("cfg(target_os = \"linux\")", "cfg(target_os = \"linux\")"),
            ("cfg(  any( unix ,windows, ) )", "cfg(any(unix, windows))"),
            ("cfg(all())", "cfg(all())"),
            ("cfg(not(all(unix, target_arch=\"x86_64\")))", "cfg(not(all(unix, target_arch = \"x86_64\")))"),
            ("cfg(r#async)", "cfg(r#async)"),
            ("cfg(true)", "cfg(true)"),
            ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"),
            ("thumbv7em-none-eabihf", "thumbv7em-none-eabihf"),
        ] {
            let platform: Platform = text.parse().unwrap();
            assert_eq!(platform.to_string(), shown, "{text}");
            assert_eq!(shown.parse::<Platform>().unwrap(), platform, "{shown}");
        }
        assert_eq!("cfg(true)".parse::<Platform>().unwrap(), Platform::Cfg(CfgExpr::True));
        assert_eq!("r#unix".parse::<Cfg>().unwrap(), name("unix"));
    }

    #[test]
    fn matches_a_target() {
        let cfg = [name("unix"), pair("target_os", "linux"), pair("target_feature", "sse2")];
        let holds = |text: &str| text.parse::<Platform>().unwrap().matches("x86_64-unknown-linux-gnu", &cfg);
        assert!(holds("cfg(unix)"));
        assert!(!holds("cfg(windows)"));
        assert!(holds("cfg(all(unix, target_os = \"linux\", not(windows)))"));
        assert!(!holds("cfg(any())"));
        assert!(holds("cfg(all())"));
        assert!(holds("cfg(any(windows, target_feature = \"sse2\"))"));
        assert!(holds("x86_64-unknown-linux-gnu"));
        assert!(!holds("aarch64-apple-darwin"));
        assert!(CfgExpr::matches_key("cfg(unix)", &cfg));
        assert!(!CfgExpr::matches_key("cfg(unix", &cfg));
        assert!(!CfgExpr::matches_key("unix", &cfg));
    }

    #[test]
    fn refusals_name_the_byte() {
        let err = |text: &str| text.parse::<Platform>().unwrap_err();
        let e = err("cfg(unix windows)");
        assert_eq!((e.text.as_str(), e.at, e.what), ("unix windows", 5, CfgWhat::Trailing));
        assert_eq!(
            e.to_string(),
            "failed to parse `unix windows` as a cfg expression: unexpected content `windows` \
             found after cfg expression (at byte 5)"
        );
        assert_eq!((err("cfg(foo = \"bar)").at, err("cfg(foo = \"bar)").what), (6, CfgWhat::UnterminatedString));
        assert_eq!(err("cfg(not(unix)").what, CfgWhat::Incomplete { expected: "`)`" });
        assert_eq!(
            err("cfg(foo = bar)").what,
            CfgWhat::UnexpectedToken { expected: "a string", found: "an identifier" }
        );
        assert_eq!(err("cfg(a, b)").what, CfgWhat::Trailing);
        assert_eq!(err("cfg()").what, CfgWhat::Incomplete { expected: "start of a cfg expression" });
        assert_eq!((err("cfg(r#1)").at, err("cfg(r#1)").what), (2, CfgWhat::UnexpectedChar('1')));
        assert_eq!((err("x86 64").at, err("x86 64").what), (3, CfgWhat::TargetChar(' ')));
        assert_eq!(err("foo(bar)").what, CfgWhat::TargetParen);
    }

    #[test]
    fn warns_of_unselectable_cfgs_and_keywords() {
        let mut warnings = Vec::new();
        let p: Platform = "cfg(any(test, feature = \"x\", unix))".parse().unwrap();
        p.check_cfg_attributes(&mut warnings);
        assert_eq!(warnings.len(), 2);
        let mut warnings = Vec::new();
        let p: Platform = "cfg(any(true = \"a\", r#false))".parse().unwrap();
        p.check_cfg_keywords(&mut warnings, Path::new("Cargo.toml"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("[Cargo.toml] future-incompatibility: `cfg(true = \"a\")`"));
    }
}
