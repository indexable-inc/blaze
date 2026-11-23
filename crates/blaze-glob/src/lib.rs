//! Unix shell patterns as cargo reads `workspace.members`: glob 0.3's syntax, its matches and
//! its errors under its default options (case-sensitive, `*` and `?` match a `/`, a leading `.`
//! is matched by a wildcard), and [`glob`], the walk that expands one over a [`Tree`] in glob's
//! order. `?` is one character, `*` any run, `**` a whole component of any depth, `[ab]`,
//! `[a-z]` and `[!ab]` one character in or out of a class (`[]]` and `[!]]` hold `]`).
//!
//! A match is a table over (token, character) positions, so a pattern of `t` tokens over a text
//! of `n` characters takes `O(t * n)` however many `*` it has; glob's own matcher backtracks.
//!
//! A [`Pattern`] is a core: it borrows its text and keeps its tokens, and the table a match
//! fills, in the allocator it was made in ([`Pattern::new_in`]). [`glob`] and [`Tree`] are not
//! yet: they speak `std::path` and `std::io::Error` and return a vector on the global heap
//! (wave-1124-sansio, W1 group a's report says what replaces them).

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(all(test, feature = "std"))]
mod tests;

use core::alloc::Allocator;
use core::fmt;
#[cfg(feature = "std")]
use std::alloc::Global;
#[cfg(feature = "std")]
use std::path::{Component, Path, PathBuf};

use blaze_own::Vec as OwnVec;

#[inline]
fn is_separator(c: char) -> bool {
    c == '/' || c == '\\'
}

/// Why a pattern does not parse, as glob says it: `Pattern syntax error near position {pos}:
/// {msg}`, `pos` a character index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatternError {
    pub pos: usize,
    pub refusal: Refusal,
}

/// What is wrong at [`PatternError::pos`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Three or more `*` in a row.
    Wildcards,
    /// `**` that is not a whole component.
    RecursiveWildcards,
    /// A `[` that opens no class.
    InvalidRange,
}

impl Refusal {
    /// glob's message.
    pub fn message(self) -> &'static str {
        match self {
            Refusal::Wildcards => "wildcards are either regular `*` or recursive `**`",
            Refusal::RecursiveWildcards => "recursive wildcards must form a single path component",
            Refusal::InvalidRange => "invalid range pattern",
        }
    }
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pattern syntax error near position {}: {}", self.pos, self.refusal.message())
    }
}

impl core::error::Error for PatternError {}

#[derive(Clone, Debug)]
enum Token<A: Allocator> {
    Char(char),
    AnyChar,
    AnySequence,
    /// `**`, with the separator after it when one followed.
    AnyRecursiveSequence,
    AnyWithin(OwnVec<Class, A>),
    AnyExcept(OwnVec<Class, A>),
}

/// By value, wherever either is allocated (a derive would ask `A: PartialEq`, which no
/// allocator is).
impl<A: Allocator> PartialEq for Token<A> {
    fn eq(&self, other: &Token<A>) -> bool {
        match (self, other) {
            (Token::Char(one), Token::Char(other)) => one == other,
            (Token::AnyChar, Token::AnyChar)
            | (Token::AnySequence, Token::AnySequence)
            | (Token::AnyRecursiveSequence, Token::AnyRecursiveSequence) => true,
            (Token::AnyWithin(one), Token::AnyWithin(other)) | (Token::AnyExcept(one), Token::AnyExcept(other)) => {
                one == other
            }
            _ => false,
        }
    }
}

impl<A: Allocator> Eq for Token<A> {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    One(char),
    Range(char, char),
}

impl Class {
    fn holds(classes: &[Class], c: char) -> bool {
        classes.iter().any(|it| match *it {
            Class::One(one) => one == c,
            Class::Range(first, last) => first <= c && c <= last,
        })
    }

    /// `[ab-z]`'s inside: a `-` between two characters makes a range, anywhere else it is one.
    fn parse<A: Allocator>(inside: &[char], alloc: A) -> OwnVec<Class, A> {
        let mut out = OwnVec::new_in(alloc);
        let mut at = 0;
        while at < inside.len() {
            if at + 3 <= inside.len() && inside[at + 1] == '-' {
                out.push(Class::Range(inside[at], inside[at + 2]));
                at += 3;
            } else {
                out.push(Class::One(inside[at]));
                at += 1;
            }
        }
        out
    }
}

/// A parsed pattern: the text it was parsed from, borrowed, and its tokens in `A`.
#[derive(Clone, Debug)]
pub struct Pattern<'a, A: Allocator> {
    tokens: OwnVec<Token<A>, A>,
    original: &'a str,
    /// A `*`, `?` or `[` (glob's `has_metachars`): matching it needs a directory listed.
    meta: bool,
}

/// Equal when parsed from equal text, wherever either is allocated.
impl<A: Allocator, B: Allocator> PartialEq<Pattern<'_, B>> for Pattern<'_, A> {
    fn eq(&self, other: &Pattern<'_, B>) -> bool {
        self.original == other.original
    }
}

impl<A: Allocator> Eq for Pattern<'_, A> {}

#[cfg(feature = "std")]
impl<'a> Pattern<'a, Global> {
    /// `pattern` parsed on the global heap, or glob's error for it.
    ///
    /// Deprecated: names no allocator; use [`Pattern::new_in`]. Goes with its last callers,
    /// blaze-manifest's layout/mod.rs:729 and :759 and blaze-new's lib.rs:1157 (and this
    /// crate's [`glob`], until it is ported).
    pub fn new(pattern: &'a str) -> Result<Pattern<'a, Global>, PatternError> {
        Pattern::new_in(pattern, Global)
    }
}

impl<'a, A: Allocator + Clone> Pattern<'a, A> {
    /// `pattern` parsed, its tokens in `alloc` (with the characters it is read through, freed
    /// on return), or glob's error for it.
    pub fn new_in(pattern: &'a str, alloc: A) -> Result<Pattern<'a, A>, PatternError> {
        let chars: OwnVec<char, A> = {
            let mut it = OwnVec::new_in(alloc.clone());
            it.extend(pattern.chars());
            it
        };
        let mut tokens = OwnVec::new_in(alloc.clone());
        let mut meta = false;
        let mut at = 0;
        while at < chars.len() {
            match chars[at] {
                '?' => {
                    meta = true;
                    tokens.push(Token::AnyChar);
                    at += 1;
                }
                '*' => {
                    meta = true;
                    let first = at;
                    while at < chars.len() && chars[at] == '*' {
                        at += 1;
                    }
                    match at - first {
                        1 => tokens.push(Token::AnySequence),
                        2 => {
                            // `**` is a whole component: after a separator or at the start, and
                            // before a separator (taken with it) or at the end.
                            if !(first == 0 || is_separator(chars[first - 1])) {
                                return Err(PatternError { pos: first - 1, refusal: Refusal::RecursiveWildcards });
                            }
                            if at < chars.len() && is_separator(chars[at]) {
                                at += 1;
                            } else if at != chars.len() {
                                return Err(PatternError { pos: at, refusal: Refusal::RecursiveWildcards });
                            }
                            // `**/**/` is one; glob keeps a second when it is the only token
                            // before it (`tokens.len() > 1`), which matches the same.
                            if !(tokens.len() > 1 && tokens.last() == Some(&Token::AnyRecursiveSequence)) {
                                tokens.push(Token::AnyRecursiveSequence);
                            }
                        }
                        _ => return Err(PatternError { pos: first + 2, refusal: Refusal::Wildcards }),
                    }
                }
                '[' => {
                    meta = true;
                    // The class's first character may be `]`, so the closing one is looked for
                    // from the one after it.
                    let negated = at + 1 < chars.len() && chars[at + 1] == '!';
                    let (from, least) = if negated { (at + 2, at + 4) } else { (at + 1, at + 3) };
                    let close = (least <= chars.len())
                        .then(|| chars[from + 1..].iter().position(|it| *it == ']'))
                        .flatten();
                    let Some(close) = close.map(|it| from + 1 + it) else {
                        return Err(PatternError { pos: at, refusal: Refusal::InvalidRange });
                    };
                    let classes = Class::parse(&chars[from..close], alloc.clone());
                    tokens.push(if negated { Token::AnyExcept(classes) } else { Token::AnyWithin(classes) });
                    at = close + 1;
                }
                c => {
                    tokens.push(Token::Char(c));
                    at += 1;
                }
            }
        }
        Ok(Pattern { tokens, original: pattern, meta })
    }

    /// The text it was parsed from.
    pub fn as_str(&self) -> &'a str {
        self.original
    }

    /// Whether it has a `*`, `?` or `[`.
    pub fn has_metachars(&self) -> bool {
        self.meta
    }

    /// Whether `text` matches it whole. Its characters and the match's two rows of `n + 1`
    /// flags are in the pattern's allocator, freed on return.
    pub fn matches(&self, text: &str) -> bool {
        let alloc = self.tokens.allocator();
        let chars: OwnVec<char, A> = {
            let mut it = OwnVec::new_in(alloc.clone());
            it.extend(text.chars());
            it
        };
        let n = chars.len();
        // `rest[p]`: the tokens from the one at hand on match `chars[p..]`; filled from the last
        // token back, starting from the empty pattern, which matches only the empty rest.
        let mut rest = OwnVec::new_in(alloc.clone());
        rest.resize(n + 1, false);
        rest[n] = true;
        let mut next = OwnVec::new_in(alloc.clone());
        next.resize(n + 1, false);
        for token in self.tokens.iter().rev() {
            core::mem::swap(&mut rest, &mut next);
            // `next` is the tokens after this one; `rest` becomes this one's.
            match token {
                Token::AnySequence => {
                    let mut any = false;
                    for p in (0..=n).rev() {
                        any |= next[p];
                        rest[p] = any;
                    }
                }
                Token::AnyRecursiveSequence => {
                    // Matches empty, a run ending just after a separator, or the whole rest of
                    // the text: glob's loop, finding no separator it can stop after, runs off
                    // the end and hands the tokens after it the empty rest, so a trailing `**`
                    // (or `**/`) takes `a/b/c` and `a` alike.
                    let mut any = next[n];
                    for p in (0..=n).rev() {
                        rest[p] = next[p] || any;
                        if p > 0 && is_separator(chars[p - 1]) {
                            any |= next[p];
                        }
                    }
                }
                one => {
                    rest[n] = false;
                    for p in 0..n {
                        let c = chars[p];
                        let takes = match one {
                            Token::Char(it) => *it == c,
                            Token::AnyChar => true,
                            Token::AnyWithin(classes) => Class::holds(classes, c),
                            Token::AnyExcept(classes) => !Class::holds(classes, c),
                            Token::AnySequence | Token::AnyRecursiveSequence => false,
                        };
                        rest[p] = takes && next[p + 1];
                    }
                }
            }
        }
        rest[0]
    }

    /// Whether its first token is a literal `.`, so that it may match `.` and `..`.
    #[cfg(feature = "std")]
    fn leading_dot(&self) -> bool {
        self.tokens.first() == Some(&Token::Char('.'))
    }

    #[cfg(feature = "std")]
    fn recursive(&self) -> bool {
        self.tokens.len() == 1 && self.tokens[0] == Token::AnyRecursiveSequence
    }
}

impl<A: Allocator> fmt::Display for Pattern<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.original)
    }
}

#[cfg(feature = "std")]
mod walk_std {
    use super::*;

/// What [`glob`] walks: a tree of directories, read through this.
pub trait Tree {
    /// Whether anything is at `path` (a dangling link counts, as glob's `symlink_metadata`).
    fn exists(&self, path: &Path) -> bool;
    /// Whether `path` is a directory, following links.
    fn is_dir(&self, path: &Path) -> bool;
    /// The names in a directory, sorted, each with whether it is a directory (not following a
    /// link); names that are not UTF-8 are left out.
    fn list(&self, path: &Path) -> std::io::Result<Vec<(String, bool), Global>>;
}

/// A directory of a pattern that could not be listed, as glob says it.
#[derive(Debug)]
pub struct GlobError {
    pub path: PathBuf,
    pub error: std::io::Error,
}

impl fmt::Display for GlobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "attempting to read `{}` resulted in an error: {}", self.path.display(), self.error)
    }
}

impl std::error::Error for GlobError {
    // dyn: std's error chain, read only on an error path.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// A path to match against the parts from `at` on; `usize::MAX` for one already matched whole.
struct Todo {
    path: PathBuf,
    dir: bool,
    at: usize,
}

/// The paths of `tree` that `pattern` matches, in glob's (alphabetical) order, with a
/// [`GlobError`] for each directory that could not be listed: glob 0.3's `glob_with` and
/// `fill_todo` under its default options.
pub fn glob<T: Tree>(tree: &T, pattern: &str) -> Result<Vec<Result<PathBuf, GlobError>, Global>, PatternError> {
    Pattern::new(pattern)?;
    let mut components = Path::new(pattern).components().peekable();
    while matches!(components.peek(), Some(Component::Prefix(..) | Component::RootDir)) {
        components.next();
    }
    let rest: PathBuf = components.map(|c| c.as_os_str()).collect();
    let normalized: PathBuf = Path::new(pattern).iter().collect();
    let root_len = normalized.as_os_str().len() - rest.as_os_str().len();
    let scope = if root_len > 0 { PathBuf::from(&pattern[..root_len]) } else { PathBuf::from(".") };

    let mut parts = OwnVec::new_in(Global);
    for component in pattern[root_len.min(pattern.len())..].split_terminator(is_separator) {
        parts.push(Pattern::new(component)?);
    }
    if root_len == pattern.len() {
        parts.push(Pattern::new("")?);
    }
    let require_dir = pattern.chars().next_back().is_some_and(is_separator);

    let mut out = Vec::new_in(Global);
    let mut todo: OwnVec<Result<Todo, GlobError>, Global> = OwnVec::new_in(Global);
    let scope_dir = tree.is_dir(&scope);
    fill(tree, &mut todo, &parts, 0, &scope, scope_dir);
    while let Some(next) = todo.pop() {
        let Todo { path, dir, mut at } = match next {
            Ok(next) => next,
            Err(e) => {
                out.push(Err(e));
                continue;
            }
        };
        if at == usize::MAX {
            if !require_dir || dir {
                out.push(Ok(path));
            }
            continue;
        }
        if parts[at].recursive() {
            let mut last = at;
            while last + 1 < parts.len() && parts[last + 1].recursive() {
                last += 1;
            }
            if dir {
                fill(tree, &mut todo, &parts, last, &path, dir);
                if last == parts.len() - 1 {
                    out.push(Ok(path));
                    continue;
                }
                at = last + 1;
            } else if last == parts.len() - 1 {
                continue;
            } else {
                at = last + 1;
            }
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
        if parts[at].matches(name) {
            if at == parts.len() - 1 {
                if !require_dir || dir {
                    out.push(Ok(path));
                }
            } else {
                fill(tree, &mut todo, &parts, at + 1, &path, dir);
            }
        }
    }
    Ok(out)
}

fn fill<T: Tree>(tree: &T, todo: &mut OwnVec<Result<Todo, GlobError>, Global>, parts: &[Pattern<'_, Global>], at: usize, path: &Path, dir: bool) {
    let add = |todo: &mut OwnVec<Result<Todo, GlobError>, Global>, next: PathBuf| {
        let next_dir = tree.is_dir(&next);
        if at + 1 == parts.len() {
            todo.push(Ok(Todo { path: next, dir: next_dir, at: usize::MAX }));
        } else {
            fill(tree, todo, parts, at + 1, &next, next_dir);
        }
    };
    let part = &parts[at];
    let curdir = path == Path::new(".");
    if !part.meta {
        let s = part.as_str();
        let special = s == "." || s == "..";
        let next = if curdir { PathBuf::from(s) } else { path.join(s) };
        if (special && dir) || (!special && tree.exists(&next)) {
            add(todo, next);
        }
    } else if dir {
        match tree.list(path) {
            Ok(names) => {
                for (name, is_dir) in names.into_iter().rev() {
                    let child = if curdir { PathBuf::from(&name) } else { path.join(&name) };
                    // glob follows a link to learn whether it is a directory.
                    let child_dir = is_dir || tree.is_dir(&child);
                    todo.push(Ok(Todo { path: child, dir: child_dir, at }));
                }
                if part.leading_dot() {
                    for special in [".", ".."] {
                        if part.matches(special) {
                            add(todo, path.join(special));
                        }
                    }
                }
            }
            Err(error) => todo.push(Err(GlobError { path: path.to_path_buf(), error })),
        }
    }
}
}

#[cfg(feature = "std")]
pub use walk_std::{GlobError, Tree, glob};
