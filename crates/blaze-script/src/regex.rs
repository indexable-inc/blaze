//! Regular expressions for the scripts and tools: a Pike VM over a parsed pattern, linear in
//! the text, with the subset of the `regex` crate's interface the tree uses (`Regex`,
//! `Captures`, `Match`, `escape`, `replace_all` with `$1`, `$name`, `${name}` and `$$`).
//!
//! Syntax: literals and escapes (`\n \t \r \f \v \a`, `\xHH`, `\x{H..}`, any escaped ASCII
//! punctuation but `<` and `>`), `.`, classes (`[a-z]`, `[^...]`, a leading `]`, escapes and the
//! Perl classes inside), `\d \w \s \D \W \S`, `\b \B`, `\A \z ^ $`, groups (capturing,
//! `(?:...)`, `(?P<name>...)`, `(?<name>...)`), flags `i`, `m`, `s` as `(?flags)` for the rest
//! of the group and `(?flags:...)`, alternation, and `* + ? {n} {n,} {n,m}`, greedy or lazy
//! (`?` after). Refused with a typed [`Error`]: backreferences, lookaround, `\p{..}` classes,
//! nested classes and class set operations (`&&`, `--`, `~~`), other flags, nesting past
//! [`MAX_NEST`], and a program past its instruction limit ([`Regex::with_limit`]).
//!
//! Semantics: leftmost-first, as the `regex` crate and Perl: of the matches starting leftmost,
//! the one the pattern's priorities prefer (earlier alternative, greedy takes more). Matching is
//! by `char` over UTF-8, positions are byte offsets. `$` without `m` is the end of the text
//! (`\z`), with `m` before any `\n`; `^` likewise. `x*` is compiled as `(?:x+)?`, so an
//! iteration that matches empty ends the repetition, and is kept only when it is the first.
//! `find_iter` and `captures_iter` do not overlap, and skip an empty match that starts where the
//! previous match ended, as the `regex` crate does.
//!
//! Character classes, chosen to be Python's (the scripts were ported from Python's `re`):
//! `\w` is [`crate::pystr::is_word`] (`char::is_alphanumeric` or `_`), `\s` is
//! `char::is_whitespace` (Unicode White_Space), `\d` is Unicode Nd
//! (`blaze_unicode::is_decimal_number`); `\b` is a change of `\w`. Under `i` a literal matches
//! a character with the same [`fold`] (its uppercase then lowercase, each where it is one
//! character), and a class matches `c` when it holds `c`, its one-character lowercase or
//! uppercase, or its fold: `ſ` and `s`, `K` (Kelvin) and `k` are one.

use std::{
    borrow::Cow,
    fmt,
    ops::{Index, Range},
    sync::Arc,
};

#[cfg(test)]
mod tests;

/// The instructions [`Regex::new`] allows a program.
pub const DEFAULT_LIMIT: usize = 1 << 16;

/// The deepest nesting of groups and repetitions a pattern may have.
pub const MAX_NEST: u32 = 250;

/// Why a pattern was refused, and where in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pattern: String,
    at: usize,
    kind: ErrorKind,
}

/// What is wrong with a pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    UnclosedGroup,
    UnopenedGroup,
    UnclosedClass,
    NestedClass,
    ClassSetOperation,
    BadRange { from: char, to: char },
    PerlClassInRange,
    BadEscape(char),
    TrailingBackslash,
    BadHex,
    Backreference,
    Lookaround,
    UnknownFlag(char),
    EmptyFlags,
    RepetitionMissing,
    BadRepetition,
    RepetitionRange { min: u32, max: u32 },
    BadGroupName,
    DuplicateName(String),
    UnicodeClass,
    TooDeep,
    TooBig { limit: usize },
}

impl Error {
    /// What is wrong.
    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    /// The byte of the pattern the error is at.
    pub fn at(&self) -> usize {
        self.at
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::UnclosedGroup => f.write_str("a group is not closed"),
            ErrorKind::UnopenedGroup => f.write_str("a `)` closes no group"),
            ErrorKind::UnclosedClass => f.write_str("a class is not closed"),
            ErrorKind::NestedClass => f.write_str("a nested class (escape the `[`)"),
            ErrorKind::ClassSetOperation => f.write_str("a class set operation (`&&`, `--`, `~~`) is not supported"),
            ErrorKind::BadRange { from, to } => write!(f, "the class range `{from}-{to}` is backwards"),
            ErrorKind::PerlClassInRange => f.write_str("a Perl class cannot end a range"),
            ErrorKind::BadEscape(c) => write!(f, "`\\{c}` is no escape"),
            ErrorKind::TrailingBackslash => f.write_str("the pattern ends in a `\\`"),
            ErrorKind::BadHex => f.write_str("a `\\x` escape is not a character's hex"),
            ErrorKind::Backreference => f.write_str("backreferences are not supported"),
            ErrorKind::Lookaround => f.write_str("lookaround is not supported"),
            ErrorKind::UnknownFlag(c) => write!(f, "the flag `{c}` is not supported (only `i`, `m`, `s`)"),
            ErrorKind::EmptyFlags => f.write_str("a flag group names no flag"),
            ErrorKind::RepetitionMissing => f.write_str("a repetition repeats nothing"),
            ErrorKind::BadRepetition => f.write_str("a counted repetition is not `{n}`, `{n,}` or `{n,m}`"),
            ErrorKind::RepetitionRange { min, max } => write!(f, "the repetition `{{{min},{max}}}` is backwards"),
            ErrorKind::BadGroupName => f.write_str("a group name is not a letter or `_` then letters, digits, `_`, `.`, `[`, `]`"),
            ErrorKind::DuplicateName(name) => write!(f, "two groups are named `{name}`"),
            ErrorKind::UnicodeClass => f.write_str("Unicode classes (`\\p`, `\\P`) are not supported"),
            ErrorKind::TooDeep => write!(f, "groups and repetitions nest deeper than {MAX_NEST}"),
            ErrorKind::TooBig { limit } => write!(f, "the compiled pattern is past its limit of {limit} instructions"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "regex `{}` at byte {}: {}", self.pattern, self.at, self.kind)
    }
}

impl std::error::Error for Error {}

/// `c`'s case fold here: its uppercase, then that's lowercase, where each is one character;
/// else `c`.
pub fn fold(c: char) -> char {
    let mut upper = c.to_uppercase();
    let (Some(up), None) = (upper.next(), upper.next()) else { return c };
    let mut lower = up.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(low), None) => low,
        _ => c,
    }
}

/// `c`'s one-character lowercase, uppercase and fold, each `c` where there is none.
fn variants(c: char) -> [char; 3] {
    fn single(mut it: impl Iterator<Item = char>, c: char) -> char {
        match (it.next(), it.next()) {
            (Some(x), None) => x,
            _ => c,
        }
    }
    [single(c.to_lowercase(), c), single(c.to_uppercase(), c), fold(c)]
}

/// `text` with every character the syntax gives a meaning escaped, so it matches itself.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '#' | '&' | '-' | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    StartText,
    EndText,
    StartLine,
    EndLine,
    Word,
    NotWord,
}

impl Look {
    fn holds(self, haystack: &str, at: usize) -> bool {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at..].chars().next();
        let word = |c: Option<char>| c.is_some_and(crate::pystr::is_word);
        match self {
            Look::StartText => at == 0,
            Look::EndText => at == haystack.len(),
            Look::StartLine => before.is_none_or(|c| c == '\n'),
            Look::EndLine => after.is_none_or(|c| c == '\n'),
            Look::Word => word(before) != word(after),
            Look::NotWord => word(before) == word(after),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Perl {
    Digit,
    Word,
    Space,
}

impl Perl {
    fn holds(self, c: char) -> bool {
        match self {
            Perl::Digit => blaze_unicode::is_decimal_number(c),
            Perl::Word => crate::pystr::is_word(c),
            Perl::Space => c.is_whitespace(),
        }
    }
}

/// A class: sorted, merged ranges and Perl classes (each maybe negated), the whole maybe
/// negated, and whether case folds.
#[derive(Clone, Debug)]
struct Class {
    ranges: Vec<(char, char)>,
    perls: Vec<(Perl, bool)>,
    negated: bool,
    fold: bool,
}

impl Class {
    fn perl(perl: Perl, negated: bool) -> Class {
        Class { ranges: Vec::new(), perls: vec![(perl, negated)], negated: false, fold: false }
    }

    fn inside(&self, c: char) -> bool {
        let in_range = self
            .ranges
            .binary_search_by(|&(lo, hi)| {
                if hi < c {
                    std::cmp::Ordering::Less
                } else if lo > c {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok();
        in_range || self.perls.iter().any(|&(perl, negated)| perl.holds(c) != negated)
    }

    fn holds(&self, c: char) -> bool {
        let found = self.inside(c) || (self.fold && variants(c).into_iter().any(|v| self.inside(v)));
        found != self.negated
    }
}

/// A parsed pattern.
enum Node {
    Empty,
    Char(char),
    /// A character under `i`: what it folds to.
    Fold(char),
    Any {
        newline: bool,
    },
    /// An index into the parser's classes.
    Class(usize),
    Look(Look),
    /// A group, with its capture index when it captures.
    Group(Option<usize>, Box<Node>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
    },
}

impl Node {
    /// How deep the node nests; every node the parser builds is at most [`MAX_NEST`] + 1 deep,
    /// so this recursion is bounded.
    fn height(&self) -> u32 {
        match self {
            Node::Group(_, inner) => inner.height() + 1,
            Node::Repeat { node, .. } => node.height() + 1,
            Node::Concat(nodes) | Node::Alt(nodes) => nodes.iter().map(Node::height).max().unwrap_or(0) + 1,
            _ => 0,
        }
    }
}

fn concat(mut nodes: Vec<Node>) -> Node {
    match nodes.len() {
        0 => Node::Empty,
        1 => nodes.pop().unwrap_or(Node::Empty),
        _ => Node::Concat(nodes),
    }
}

#[derive(Clone, Copy, Default)]
struct Flags {
    fold: bool,
    multi_line: bool,
    dot_newline: bool,
}

enum ClassAtom {
    Char(char),
    Perl(Perl, bool),
}

struct Parser<'p> {
    pattern: &'p str,
    at: usize,
    names: Vec<Option<String>>,
    classes: Vec<Class>,
}

impl<'p> Parser<'p> {
    fn error(&self, at: usize, kind: ErrorKind) -> Error {
        Error { pattern: self.pattern.to_owned(), at, kind }
    }

    fn rest(&self) -> &'p str {
        &self.pattern[self.at..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, text: &str) -> bool {
        if self.rest().starts_with(text) {
            self.at += text.len();
            true
        } else {
            false
        }
    }

    fn literal(c: char, flags: Flags) -> Node {
        if flags.fold { Node::Fold(fold(c)) } else { Node::Char(c) }
    }

    /// Alternatives up to a `)` or the end, which it leaves.
    fn alternation(&mut self, mut flags: Flags, depth: u32) -> Result<Node, Error> {
        if depth > MAX_NEST {
            return Err(self.error(self.at, ErrorKind::TooDeep));
        }
        let mut branches = Vec::new();
        let mut seq: Vec<Node> = Vec::new();
        while let Some(c) = self.peek() {
            let start = self.at;
            match c {
                ')' => break,
                '|' => {
                    self.bump();
                    branches.push(concat(std::mem::take(&mut seq)));
                }
                '*' | '+' | '?' | '{' => {
                    let (min, max) = self.repetition(start)?;
                    let greedy = !self.eat("?");
                    let Some(last) = seq.pop() else {
                        return Err(self.error(start, ErrorKind::RepetitionMissing));
                    };
                    if depth + last.height() + 1 > MAX_NEST {
                        return Err(self.error(start, ErrorKind::TooDeep));
                    }
                    seq.push(Node::Repeat { node: Box::new(last), min, max, greedy });
                }
                '(' => {
                    self.bump();
                    if let Some(group) = self.group(start, &mut flags, depth)? {
                        seq.push(group);
                    }
                }
                '[' => {
                    self.bump();
                    let class = self.class(start, flags)?;
                    self.classes.push(class);
                    seq.push(Node::Class(self.classes.len() - 1));
                }
                '.' => {
                    self.bump();
                    seq.push(Node::Any { newline: flags.dot_newline });
                }
                '^' => {
                    self.bump();
                    seq.push(Node::Look(if flags.multi_line { Look::StartLine } else { Look::StartText }));
                }
                '$' => {
                    self.bump();
                    seq.push(Node::Look(if flags.multi_line { Look::EndLine } else { Look::EndText }));
                }
                '\\' => {
                    self.bump();
                    let node = self.escape(start, flags)?;
                    seq.push(node);
                }
                _ => {
                    self.bump();
                    seq.push(Parser::literal(c, flags));
                }
            }
        }
        branches.push(concat(seq));
        Ok(if branches.len() == 1 { branches.pop().unwrap_or(Node::Empty) } else { Node::Alt(branches) })
    }

    fn number(&mut self, start: usize) -> Result<u32, Error> {
        let digits = self.rest().bytes().take_while(u8::is_ascii_digit).count();
        let text = &self.rest()[..digits];
        self.at += digits;
        text.parse::<u32>().map_err(|_| self.error(start, ErrorKind::BadRepetition))
    }

    fn repetition(&mut self, start: usize) -> Result<(u32, Option<u32>), Error> {
        match self.bump() {
            Some('*') => Ok((0, None)),
            Some('+') => Ok((1, None)),
            Some('?') => Ok((0, Some(1))),
            _ => {
                let min = self.number(start)?;
                let max = if self.eat(",") {
                    if self.peek() == Some('}') { None } else { Some(self.number(start)?) }
                } else {
                    Some(min)
                };
                if !self.eat("}") {
                    return Err(self.error(start, ErrorKind::BadRepetition));
                }
                if let Some(max) = max
                    && max < min
                {
                    return Err(self.error(start, ErrorKind::RepetitionRange { min, max }));
                }
                Ok((min, max))
            }
        }
    }

    /// After a `(`: the group, or `None` for a flag group `(?i)` that changed `flags`.
    fn group(&mut self, start: usize, flags: &mut Flags, depth: u32) -> Result<Option<Node>, Error> {
        let mut name = None;
        if self.eat("?") {
            let rest = self.rest();
            if rest.starts_with('=') || rest.starts_with('!') || rest.starts_with("<=") || rest.starts_with("<!") {
                return Err(self.error(start, ErrorKind::Lookaround));
            }
            if rest.starts_with("P=") || rest.starts_with("P>") {
                return Err(self.error(start, ErrorKind::Backreference));
            }
            if self.eat("P<") || self.eat("<") {
                let Some(end) = self.rest().find('>') else {
                    return Err(self.error(start, ErrorKind::BadGroupName));
                };
                let text = &self.rest()[..end];
                let mut chars = text.chars();
                let good = chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
                    && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '[' | ']'));
                if !good {
                    return Err(self.error(start, ErrorKind::BadGroupName));
                }
                if self.names.iter().any(|it| it.as_deref() == Some(text)) {
                    return Err(self.error(start, ErrorKind::DuplicateName(text.to_owned())));
                }
                self.at += end + 1;
                name = Some(text.to_owned());
            } else {
                let mut new = *flags;
                let mut negate = false;
                let mut any = false;
                loop {
                    let flag_at = self.at;
                    match self.bump() {
                        Some('i') => new.fold = !negate,
                        Some('m') => new.multi_line = !negate,
                        Some('s') => new.dot_newline = !negate,
                        Some('-') if !negate => {
                            negate = true;
                            continue;
                        }
                        // `(?:a)` is a plain non-capturing group; `(?-:a)` names no flag.
                        Some(':') if any || !negate => {
                            let inner = self.alternation(new, depth + 1)?;
                            if !self.eat(")") {
                                return Err(self.error(start, ErrorKind::UnclosedGroup));
                            }
                            return self.wrap(start, depth, None, inner).map(Some);
                        }
                        Some(')') if any => {
                            *flags = new;
                            return Ok(None);
                        }
                        Some(':' | ')' | '-') => return Err(self.error(flag_at, ErrorKind::EmptyFlags)),
                        Some(c) => return Err(self.error(flag_at, ErrorKind::UnknownFlag(c))),
                        None => return Err(self.error(start, ErrorKind::UnclosedGroup)),
                    }
                    any = true;
                }
            }
        }
        let index = self.names.len();
        self.names.push(name);
        let inner = self.alternation(*flags, depth + 1)?;
        if !self.eat(")") {
            return Err(self.error(start, ErrorKind::UnclosedGroup));
        }
        self.wrap(start, depth, Some(index), inner).map(Some)
    }

    fn wrap(&self, start: usize, depth: u32, index: Option<usize>, inner: Node) -> Result<Node, Error> {
        if depth + inner.height() + 1 > MAX_NEST {
            return Err(self.error(start, ErrorKind::TooDeep));
        }
        Ok(Node::Group(index, Box::new(inner)))
    }

    /// After a `\`, outside a class.
    fn escape(&mut self, start: usize, flags: Flags) -> Result<Node, Error> {
        let Some(c) = self.bump() else {
            return Err(self.error(start, ErrorKind::TrailingBackslash));
        };
        let perl = |classes: &mut Vec<Class>, perl: Perl| {
            classes.push(Class::perl(perl, c.is_ascii_uppercase()));
            Node::Class(classes.len() - 1)
        };
        Ok(match c {
            'd' | 'D' => perl(&mut self.classes, Perl::Digit),
            'w' | 'W' => perl(&mut self.classes, Perl::Word),
            's' | 'S' => perl(&mut self.classes, Perl::Space),
            'b' => Node::Look(Look::Word),
            'B' => Node::Look(Look::NotWord),
            'A' => Node::Look(Look::StartText),
            'z' => Node::Look(Look::EndText),
            'p' | 'P' => return Err(self.error(start, ErrorKind::UnicodeClass)),
            '0'..='9' => return Err(self.error(start, ErrorKind::Backreference)),
            _ => Parser::literal(self.escaped_char(start, c)?, flags),
        })
    }

    /// The character `\c` stands for, inside a class or out.
    fn escaped_char(&mut self, start: usize, c: char) -> Result<char, Error> {
        Ok(match c {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            'f' => '\u{c}',
            'v' => '\u{b}',
            'a' => '\u{7}',
            'x' => {
                let digits = if self.eat("{") {
                    let Some(end) = self.rest().find('}') else {
                        return Err(self.error(start, ErrorKind::BadHex));
                    };
                    let digits = &self.rest()[..end];
                    self.at += end + 1;
                    digits
                } else {
                    let Some(digits) = self.rest().get(..2) else {
                        return Err(self.error(start, ErrorKind::BadHex));
                    };
                    self.at += 2;
                    digits
                };
                let good = !digits.is_empty() && digits.len() <= 8 && digits.bytes().all(|b| b.is_ascii_hexdigit());
                let value = if good { u32::from_str_radix(digits, 16).ok() } else { None };
                match value.and_then(char::from_u32) {
                    Some(c) => c,
                    None => return Err(self.error(start, ErrorKind::BadHex)),
                }
            }
            '<' | '>' => return Err(self.error(start, ErrorKind::BadEscape(c))),
            c if c.is_ascii_punctuation() => c,
            c => return Err(self.error(start, ErrorKind::BadEscape(c))),
        })
    }

    fn class_atom(&mut self, start: usize) -> Result<ClassAtom, Error> {
        let escape_at = self.at;
        match self.bump() {
            None => Err(self.error(start, ErrorKind::UnclosedClass)),
            Some('\\') => {
                let Some(c) = self.bump() else {
                    return Err(self.error(escape_at, ErrorKind::TrailingBackslash));
                };
                let negated = c.is_ascii_uppercase();
                match c {
                    'd' | 'D' => Ok(ClassAtom::Perl(Perl::Digit, negated)),
                    'w' | 'W' => Ok(ClassAtom::Perl(Perl::Word, negated)),
                    's' | 'S' => Ok(ClassAtom::Perl(Perl::Space, negated)),
                    'p' | 'P' => Err(self.error(escape_at, ErrorKind::UnicodeClass)),
                    _ => Ok(ClassAtom::Char(self.escaped_char(escape_at, c)?)),
                }
            }
            Some(c) => Ok(ClassAtom::Char(c)),
        }
    }

    /// After a `[`.
    fn class(&mut self, start: usize, flags: Flags) -> Result<Class, Error> {
        let negated = self.eat("^");
        let mut ranges = Vec::new();
        let mut perls = Vec::new();
        let mut first = true;
        loop {
            let item_at = self.at;
            let Some(c) = self.peek() else {
                return Err(self.error(start, ErrorKind::UnclosedClass));
            };
            if c == ']' && !first {
                self.bump();
                break;
            }
            first = false;
            if c == '[' {
                return Err(self.error(item_at, ErrorKind::NestedClass));
            }
            let rest = self.rest();
            if rest.starts_with("&&") || rest.starts_with("~~") || rest.starts_with("--") {
                return Err(self.error(item_at, ErrorKind::ClassSetOperation));
            }
            match self.class_atom(start)? {
                ClassAtom::Perl(perl, negated) => perls.push((perl, negated)),
                ClassAtom::Char(lo) => {
                    let rest = self.rest();
                    if rest.starts_with('-') && !rest.starts_with("-]") && !rest.starts_with("--") {
                        self.bump();
                        let hi_at = self.at;
                        match self.class_atom(start)? {
                            ClassAtom::Char(hi) if hi >= lo => ranges.push((lo, hi)),
                            ClassAtom::Char(hi) => {
                                return Err(self.error(item_at, ErrorKind::BadRange { from: lo, to: hi }));
                            }
                            ClassAtom::Perl(..) => return Err(self.error(hi_at, ErrorKind::PerlClassInRange)),
                        }
                    } else {
                        ranges.push((lo, lo));
                    }
                }
            }
        }
        ranges.sort_unstable();
        let mut merged: Vec<(char, char)> = Vec::with_capacity(ranges.len());
        for (lo, hi) in ranges {
            match merged.last_mut() {
                Some(last) if u32::from(lo) <= u32::from(last.1).saturating_add(1) => last.1 = last.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        Ok(Class { ranges: merged, perls, negated, fold: flags.fold })
    }
}

#[derive(Clone, Copy, Debug)]
enum Inst {
    Char(char),
    Fold(char),
    Any,
    AnyButNewline,
    Class(usize),
    Look(Look),
    /// Both, the first preferred.
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    Match,
}

struct Program {
    insts: Vec<Inst>,
    classes: Vec<Class>,
    /// Each group's name, group 0 the whole match.
    names: Vec<Option<String>>,
    /// Every match starts at the text's start.
    anchored: bool,
}

struct Compiler<'p> {
    pattern: &'p str,
    insts: Vec<Inst>,
    limit: usize,
}

impl Compiler<'_> {
    fn push(&mut self, inst: Inst) -> Result<usize, Error> {
        if self.insts.len() >= self.limit {
            return Err(Error { pattern: self.pattern.to_owned(), at: 0, kind: ErrorKind::TooBig { limit: self.limit } });
        }
        self.insts.push(inst);
        Ok(self.insts.len() - 1)
    }

    fn split(greedy: bool, body: usize, end: usize) -> Inst {
        if greedy { Inst::Split(body, end) } else { Inst::Split(end, body) }
    }

    fn node(&mut self, node: &Node) -> Result<(), Error> {
        match node {
            Node::Empty => {}
            Node::Char(c) => {
                self.push(Inst::Char(*c))?;
            }
            Node::Fold(c) => {
                self.push(Inst::Fold(*c))?;
            }
            Node::Any { newline } => {
                self.push(if *newline { Inst::Any } else { Inst::AnyButNewline })?;
            }
            Node::Class(index) => {
                self.push(Inst::Class(*index))?;
            }
            Node::Look(look) => {
                self.push(Inst::Look(*look))?;
            }
            Node::Group(Some(index), inner) => {
                self.push(Inst::Save(2 * index))?;
                self.node(inner)?;
                self.push(Inst::Save(2 * index + 1))?;
            }
            Node::Group(None, inner) => self.node(inner)?,
            Node::Concat(nodes) => {
                for node in nodes {
                    self.node(node)?;
                }
            }
            Node::Alt(branches) => {
                let mut jumps = Vec::with_capacity(branches.len());
                for (at, branch) in branches.iter().enumerate() {
                    if at + 1 < branches.len() {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.node(branch)?;
                        jumps.push(self.push(Inst::Jmp(0))?);
                        self.insts[split] = Inst::Split(split + 1, self.insts.len());
                    } else {
                        self.node(branch)?;
                    }
                }
                let end = self.insts.len();
                for jump in jumps {
                    self.insts[jump] = Inst::Jmp(end);
                }
            }
            Node::Repeat { node, min, max, greedy } => {
                for _ in 0..*min {
                    self.node(node)?;
                }
                match max {
                    // `(?:x+)?`: enter, the body, then again or out.
                    None => {
                        let entry = self.push(Inst::Split(0, 0))?;
                        self.node(node)?;
                        let back = self.push(Inst::Split(0, 0))?;
                        let end = back + 1;
                        self.insts[entry] = Compiler::split(*greedy, entry + 1, end);
                        self.insts[back] = Compiler::split(*greedy, entry + 1, end);
                    }
                    Some(max) => {
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            splits.push(self.push(Inst::Split(0, 0))?);
                            self.node(node)?;
                        }
                        let end = self.insts.len();
                        for split in splits {
                            self.insts[split] = Compiler::split(*greedy, split + 1, end);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// A set of threads in priority order, each instruction at most once, with its slots.
struct Threads {
    dense: Vec<usize>,
    sparse: Vec<usize>,
    slots: Vec<Option<usize>>,
    width: usize,
}

impl Threads {
    fn new(len: usize, width: usize) -> Threads {
        Threads { dense: Vec::with_capacity(len), sparse: vec![0; len], slots: vec![None; len * width], width }
    }

    fn contains(&self, pc: usize) -> bool {
        self.dense.get(self.sparse[pc]) == Some(&pc)
    }

    fn insert(&mut self, pc: usize) {
        self.sparse[pc] = self.dense.len();
        self.dense.push(pc);
    }

    fn slots(&self, pc: usize) -> &[Option<usize>] {
        &self.slots[pc * self.width..(pc + 1) * self.width]
    }
}

enum Frame {
    Explore(usize),
    Restore(usize, Option<usize>),
}

impl Program {
    /// Adds the threads `pc` reaches at `at` without reading, `slots` as they are there.
    fn closure(
        &self,
        threads: &mut Threads,
        stack: &mut Vec<Frame>,
        slots: &mut [Option<usize>],
        pc: usize,
        haystack: &str,
        at: usize,
    ) {
        stack.push(Frame::Explore(pc));
        while let Some(frame) = stack.pop() {
            let mut pc = match frame {
                Frame::Restore(slot, value) => {
                    slots[slot] = value;
                    continue;
                }
                Frame::Explore(pc) => pc,
            };
            loop {
                if threads.contains(pc) {
                    break;
                }
                threads.insert(pc);
                match self.insts[pc] {
                    Inst::Jmp(to) => pc = to,
                    Inst::Split(first, second) => {
                        stack.push(Frame::Explore(second));
                        pc = first;
                    }
                    Inst::Save(slot) => {
                        if slot < slots.len() {
                            stack.push(Frame::Restore(slot, slots[slot]));
                            slots[slot] = Some(at);
                        }
                        pc += 1;
                    }
                    Inst::Look(look) => {
                        if !look.holds(haystack, at) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Char(_) | Inst::Fold(_) | Inst::Any | Inst::AnyButNewline | Inst::Class(_) | Inst::Match => {
                        let width = threads.width;
                        threads.slots[pc * width..(pc + 1) * width].copy_from_slice(slots);
                        break;
                    }
                }
            }
        }
    }

    /// The leftmost-first match starting at or after `start`: its first `width` slots, or with
    /// `earliest` the first match found at all.
    fn search(&self, haystack: &str, start: usize, width: usize, earliest: bool) -> Option<Vec<Option<usize>>> {
        if self.anchored && start > 0 {
            return None;
        }
        let len = self.insts.len();
        let mut current = Threads::new(len, width);
        let mut next = Threads::new(len, width);
        let mut stack = Vec::new();
        let mut scratch = vec![None; width];
        let mut found = None;
        let mut at = start;
        loop {
            if found.is_none() && (!self.anchored || at == 0) {
                scratch.fill(None);
                self.closure(&mut current, &mut stack, &mut scratch, 0, haystack, at);
            }
            if current.dense.is_empty() && (found.is_some() || self.anchored) {
                break;
            }
            let ahead = haystack[at..].chars().next();
            for i in 0..current.dense.len() {
                let pc = current.dense[i];
                let steps = match self.insts[pc] {
                    Inst::Match => {
                        found = Some(current.slots(pc).to_vec());
                        if earliest {
                            return found;
                        }
                        // Every thread after this one is of lower priority.
                        break;
                    }
                    Inst::Char(c) => ahead == Some(c),
                    Inst::Fold(f) => ahead.is_some_and(|c| fold(c) == f),
                    Inst::Any => ahead.is_some(),
                    Inst::AnyButNewline => ahead.is_some_and(|c| c != '\n'),
                    Inst::Class(index) => ahead.is_some_and(|c| self.classes[index].holds(c)),
                    Inst::Split(..) | Inst::Jmp(_) | Inst::Save(_) | Inst::Look(_) => false,
                };
                if let Some(c) = ahead.filter(|_| steps) {
                    scratch.copy_from_slice(current.slots(pc));
                    self.closure(&mut next, &mut stack, &mut scratch, pc + 1, haystack, at + c.len_utf8());
                }
            }
            let Some(c) = ahead else { break };
            at += c.len_utf8();
            std::mem::swap(&mut current, &mut next);
            next.dense.clear();
        }
        found
    }
}

/// A compiled pattern.
#[derive(Clone)]
pub struct Regex {
    pattern: Arc<str>,
    program: Arc<Program>,
}

impl fmt::Debug for Regex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Regex").field(&&*self.pattern).finish()
    }
}

impl fmt::Display for Regex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.pattern)
    }
}

impl Regex {
    /// `pattern` compiled within [`DEFAULT_LIMIT`] instructions.
    ///
    /// # Errors
    ///
    /// An [`Error`] naming what is wrong and where.
    pub fn new(pattern: &str) -> Result<Regex, Error> {
        Regex::with_limit(pattern, DEFAULT_LIMIT)
    }

    /// `pattern` compiled within `limit` instructions: what one match holds is that many
    /// threads, each with its slots.
    ///
    /// # Errors
    ///
    /// An [`Error`] naming what is wrong and where, [`ErrorKind::TooBig`] past the limit.
    pub fn with_limit(pattern: &str, limit: usize) -> Result<Regex, Error> {
        let mut parser = Parser { pattern, at: 0, names: vec![None], classes: Vec::new() };
        let node = parser.alternation(Flags::default(), 0)?;
        if parser.at < pattern.len() {
            return Err(parser.error(parser.at, ErrorKind::UnopenedGroup));
        }
        let mut compiler = Compiler { pattern, insts: Vec::new(), limit };
        compiler.push(Inst::Save(0))?;
        compiler.node(&node)?;
        compiler.push(Inst::Save(1))?;
        compiler.push(Inst::Match)?;
        let anchored = matches!(compiler.insts.get(1), Some(Inst::Look(Look::StartText)));
        let program = Program { insts: compiler.insts, classes: parser.classes, names: parser.names, anchored };
        Ok(Regex { pattern: Arc::from(pattern), program: Arc::new(program) })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.pattern
    }

    /// The groups, the whole match (group 0) included.
    pub fn captures_len(&self) -> usize {
        self.program.names.len()
    }

    /// Each group's name, `None` for group 0 and unnamed groups.
    pub fn capture_names(&self) -> impl Iterator<Item = Option<&str>> + '_ {
        self.program.names.iter().map(Option::as_deref)
    }

    /// Whether the pattern matches anywhere in `haystack`.
    pub fn is_match(&self, haystack: &str) -> bool {
        self.program.search(haystack, 0, 0, true).is_some()
    }

    fn find_at<'h>(&self, haystack: &'h str, start: usize) -> Option<Match<'h>> {
        let slots = self.program.search(haystack, start, 2, false)?;
        Some(Match { haystack, start: slots[0]?, end: slots[1]? })
    }

    fn captures_at<'h>(&self, haystack: &'h str, start: usize) -> Option<Captures<'h>> {
        let slots = self.program.search(haystack, start, 2 * self.program.names.len(), false)?;
        Some(Captures { haystack, slots, program: Arc::clone(&self.program) })
    }

    /// The leftmost-first match.
    pub fn find<'h>(&self, haystack: &'h str) -> Option<Match<'h>> {
        self.find_at(haystack, 0)
    }

    /// The leftmost-first match with its groups.
    pub fn captures<'h>(&self, haystack: &'h str) -> Option<Captures<'h>> {
        self.captures_at(haystack, 0)
    }

    /// Every match, left to right, none overlapping.
    pub fn find_iter<'r, 'h>(&'r self, haystack: &'h str) -> Matches<'r, 'h> {
        Matches { regex: self, walk: Walk { haystack, at: 0, last_end: None } }
    }

    /// Every match with its groups, left to right, none overlapping.
    pub fn captures_iter<'r, 'h>(&'r self, haystack: &'h str) -> CaptureMatches<'r, 'h> {
        CaptureMatches { regex: self, walk: Walk { haystack, at: 0, last_end: None } }
    }

    /// `haystack` with its first match replaced, as [`Regex::replace_all`] replaces each.
    pub fn replace<'h, R: Replacer>(&self, haystack: &'h str, mut replacer: R) -> Cow<'h, str> {
        let Some(caps) = self.captures(haystack) else { return Cow::Borrowed(haystack) };
        let mut out = String::with_capacity(haystack.len());
        out.push_str(&haystack[..caps.start()]);
        replacer.replace_append(&caps, &mut out);
        out.push_str(&haystack[caps.end()..]);
        Cow::Owned(out)
    }

    /// `haystack` with every match replaced: a `&str` replacement expands `$1`, `$name`,
    /// `${name}` and `$$` ([`Captures::expand`]); a closure writes what it returns.
    pub fn replace_all<'h, R: Replacer>(&self, haystack: &'h str, mut replacer: R) -> Cow<'h, str> {
        let mut matches = self.captures_iter(haystack).peekable();
        if matches.peek().is_none() {
            return Cow::Borrowed(haystack);
        }
        let mut out = String::with_capacity(haystack.len());
        let mut last = 0;
        for caps in matches {
            let (start, end) = (caps.start(), caps.end());
            out.push_str(&haystack[last..start]);
            replacer.replace_append(&caps, &mut out);
            last = end;
        }
        out.push_str(&haystack[last..]);
        Cow::Owned(out)
    }
}

/// Where an iteration over matches is.
struct Walk<'h> {
    haystack: &'h str,
    at: usize,
    last_end: Option<usize>,
}

impl<'h> Walk<'h> {
    /// The next match's slots by `search`, skipping an empty match where the last one ended.
    fn next(&mut self, mut search: impl FnMut(&'h str, usize) -> Option<(usize, usize)>) -> Option<usize> {
        loop {
            if self.at > self.haystack.len() {
                return None;
            }
            let (start, end) = search(self.haystack, self.at)?;
            if start == end && self.last_end == Some(end) {
                let c = self.haystack[self.at..].chars().next()?;
                self.at += c.len_utf8();
                continue;
            }
            self.at = end;
            self.last_end = Some(end);
            return Some(start);
        }
    }
}

/// [`Regex::find_iter`].
pub struct Matches<'r, 'h> {
    regex: &'r Regex,
    walk: Walk<'h>,
}

impl<'h> Iterator for Matches<'_, 'h> {
    type Item = Match<'h>;

    fn next(&mut self) -> Option<Match<'h>> {
        let regex = self.regex;
        let mut found = None;
        self.walk.next(|haystack, at| {
            let m = regex.find_at(haystack, at)?;
            found = Some(m);
            Some((m.start, m.end))
        })?;
        found
    }
}

/// [`Regex::captures_iter`].
pub struct CaptureMatches<'r, 'h> {
    regex: &'r Regex,
    walk: Walk<'h>,
}

impl<'h> Iterator for CaptureMatches<'_, 'h> {
    type Item = Captures<'h>;

    fn next(&mut self) -> Option<Captures<'h>> {
        let regex = self.regex;
        let mut found = None;
        self.walk.next(|haystack, at| {
            let caps = regex.captures_at(haystack, at)?;
            let span = (caps.start(), caps.end());
            found = Some(caps);
            Some(span)
        })?;
        found
    }
}

/// What one match or group took of the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match<'h> {
    haystack: &'h str,
    start: usize,
    end: usize,
}

impl<'h> Match<'h> {
    pub fn start(&self) -> usize {
        self.start
    }

    pub fn end(&self) -> usize {
        self.end
    }

    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn as_str(&self) -> &'h str {
        &self.haystack[self.start..self.end]
    }
}

/// A match's groups.
pub struct Captures<'h> {
    haystack: &'h str,
    slots: Vec<Option<usize>>,
    program: Arc<Program>,
}

impl fmt::Debug for Captures<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter().map(|it| it.map(|m| m.as_str()))).finish()
    }
}

impl<'h> Captures<'h> {
    /// The whole match's start: group 0 is always set.
    fn start(&self) -> usize {
        self.slots[0].unwrap_or(0)
    }

    fn end(&self) -> usize {
        self.slots[1].unwrap_or(0)
    }

    /// Group `index`, when it took part in the match.
    pub fn get(&self, index: usize) -> Option<Match<'h>> {
        let start = (*self.slots.get(2 * index)?)?;
        let end = (*self.slots.get(2 * index + 1)?)?;
        Some(Match { haystack: self.haystack, start, end })
    }

    /// The group named `name`, when it took part in the match.
    pub fn name(&self, name: &str) -> Option<Match<'h>> {
        let index = self.program.names.iter().position(|it| it.as_deref() == Some(name))?;
        self.get(index)
    }

    /// The groups, group 0 included.
    pub fn len(&self) -> usize {
        self.program.names.len()
    }

    /// Each group in order, `None` where it took no part.
    pub fn iter(&self) -> impl Iterator<Item = Option<Match<'h>>> + '_ {
        (0..self.len()).map(|index| self.get(index))
    }

    /// `replacement` written to `dst` as the `regex` crate writes it: `$$` is `$`; `$name` is
    /// the longest run of ASCII letters, digits and `_` after the `$`, `${name}` what the braces
    /// hold; a name that parses as a number is that group, else the group of that name; a group
    /// that took no part, or does not exist, writes nothing; a `$` that names nothing is itself.
    pub fn expand(&self, replacement: &str, dst: &mut String) {
        let mut rest = replacement;
        while let Some(at) = rest.find('$') {
            dst.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            if let Some(after) = after.strip_prefix('$') {
                dst.push('$');
                rest = after;
                continue;
            }
            let (name, tail) = match after.strip_prefix('{') {
                Some(inner) => match inner.find('}') {
                    Some(end) => (&inner[..end], &inner[end + 1..]),
                    None => ("", after),
                },
                None => {
                    let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
                    (&after[..end], &after[end..])
                }
            };
            if name.is_empty() {
                dst.push('$');
                rest = after;
                continue;
            }
            let group = match name.parse::<usize>() {
                Ok(index) => self.get(index),
                Err(_) => self.name(name),
            };
            if let Some(group) = group {
                dst.push_str(group.as_str());
            }
            rest = tail;
        }
        dst.push_str(rest);
    }
}

impl Index<usize> for Captures<'_> {
    type Output = str;

    fn index(&self, index: usize) -> &str {
        match self.get(index) {
            Some(group) => group.as_str(),
            None => panic!("group {index} took no part in the match"),
        }
    }
}

impl Index<&str> for Captures<'_> {
    type Output = str;

    fn index(&self, name: &str) -> &str {
        match self.name(name) {
            Some(group) => group.as_str(),
            None => panic!("no group `{name}` took part in the match"),
        }
    }
}

/// What [`Regex::replace_all`] writes for a match.
pub trait Replacer {
    fn replace_append(&mut self, caps: &Captures<'_>, dst: &mut String);
}

impl Replacer for &str {
    fn replace_append(&mut self, caps: &Captures<'_>, dst: &mut String) {
        caps.expand(*self, dst);
    }
}

impl<F, T> Replacer for F
where
    F: FnMut(&Captures<'_>) -> T,
    T: AsRef<str>,
{
    fn replace_append(&mut self, caps: &Captures<'_>, dst: &mut String) {
        dst.push_str(self(caps).as_ref());
    }
}
