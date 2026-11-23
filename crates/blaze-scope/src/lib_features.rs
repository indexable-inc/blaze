//! The library features a toolchain crate declares for one configuration, read from its own
//! source text: what `#![feature(name)]` of a library feature is checked against (rustc's
//! `lib_features`: every `#[stable(feature = "..")]`, `#[unstable(..)]`,
//! `#[rustc_const_stable(..)]`, `#[rustc_const_unstable(..)]` and
//! `#[rustc_default_body_unstable(..)]` of every crate loaded; a name none declares is E0635).
//!
//! The walk follows the crate's modules as the compiler does under the crate's cfg: `mod x;`
//! by the Reference's file rules and `#[path]`, inline modules, `cfg_select!` at item level
//! (its first arm whose predicate holds), `cfg_if!` of the cfg-if crate's grammar (std's
//! `backtrace` and `sys`: each chain's first branch that holds), `include!("file")` at item level, and `impl`,
//! `trait` and `extern` blocks; a struct's, union's or enum's fields and variants are read with
//! their own attributes. An item whose `#[cfg]` (or a module whose `#![cfg]`) is false is
//! skipped whole, so a feature declared only under another target's cfg is not declared
//! (std's `wasi_ext`, under `cfg(target_env = "p1" | ..)` of `target_family = "wasm"`).
//!
//! What expansion would make is not followed: a stability attribute inside a function's body,
//! a `macro_rules!` body or another macro call's tokens is a *mention* (the item it would be on
//! exists only once the macro is called), never a declaration. A caller treats a mentioned name
//! as not known either way. A `mod x;` inside such tokens would hide a file: it is
//! [`ScanError::Unfollowed`], so the listing never claims a name is absent while a file it did
//! not read could declare it (rule 3).
//!
//! Sans-IO: the files come through [`SourceTexts`], the cfg as a [`Cfg`].

#![blaze::sans_io]

use core::{fmt, marker::PhantomData};

use blaze_own::{AString, AVec, Alloc};

use crate::resolve::SourceTexts;
use crate::task::{Cfg, joined, pop_dir};

/// The most files one crate's walk reads (rule 18): core with stdarch's `core_arch` is about
/// 1,500.
pub const MOST_FILES: usize = 100_000;
/// The most nested a module, block or group may be.
pub const MOST_DEPTH: usize = 512;

/// The attributes that declare a library feature.
pub const STABILITY: &[&str] = &["rustc_const_stable", "rustc_const_unstable", "rustc_default_body_unstable", "stable", "unstable"];

/// Why a crate's features were not listed: each names the file (its path as read) and the byte.
#[derive(Debug)]
pub enum ScanError<'a> {
    /// A file a module, `#[path]` or `include!` names that the crate does not have.
    Missing { path: AString<'a> },
    /// A cfg predicate [`Cfg::eval`] does not read.
    Cfg { file: AString<'a>, at: usize },
    /// Text that does not read as tokens: a comment, string or group that does not close.
    Text { file: AString<'a>, at: usize },
    /// A form the walk does not follow and that could hide a module: a `mod x;` in a macro's
    /// tokens, an `include!` of a computed path, a `cfg_select!` arm not in braces.
    Unfollowed { file: AString<'a>, at: usize },
    /// Nested past [`MOST_DEPTH`].
    TooDeep { file: AString<'a>, at: usize },
    /// More than [`MOST_FILES`] files.
    TooManyFiles,
}

impl fmt::Display for ScanError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Missing { path } => write!(f, "library features: the crate has no file `{path}`"),
            ScanError::Cfg { file, at } => write!(f, "library features: {file} byte {at}: a cfg predicate this does not read"),
            ScanError::Text { file, at } => write!(f, "library features: {file} byte {at}: a comment, string or group that does not close"),
            ScanError::Unfollowed { file, at } => write!(f, "library features: {file} byte {at}: a form that could hide a module (a `mod x;` in a macro's tokens, a computed include!, a cfg_select! arm without braces, a cfg_if! of another shape)"),
            ScanError::TooDeep { file, at } => write!(f, "library features: {file} byte {at}: nested past {MOST_DEPTH}"),
            ScanError::TooManyFiles => write!(f, "library features: more than {MOST_FILES} files"),
        }
    }
}

/// [`LibFeatures::new`]'s options.
pub struct LibFeaturesOpts<'a> {
    pub alloc: Alloc<'a>,
}

/// The features read so far from one or more crates (the crates one compilation loads).
pub struct LibFeatures<'t, 'a> {
    alloc: Alloc<'a>,
    declared: AVec<'a, &'t str>,
    mentioned: AVec<'a, &'t str>,
    /// The current item's attributes' features, kept until its cfg is known.
    pending: AVec<'a, &'t str>,
    files: usize,
}

/// A listing: both lists sorted, each name once, no mentioned name declared.
#[derive(Debug)]
pub struct Listed<'t, 'a> {
    pub declared: AVec<'a, &'t str>,
    pub mentioned: AVec<'a, &'t str>,
    /// The files read.
    pub files: usize,
}

impl<'t, 'a> LibFeatures<'t, 'a> {
    pub fn new(opts: LibFeaturesOpts<'a>) -> LibFeatures<'t, 'a> {
        let alloc = opts.alloc;
        LibFeatures { declared: AVec::new_in(alloc), mentioned: AVec::new_in(alloc), pending: AVec::new_in(alloc), alloc, files: 0 }
    }

    /// Reads the crate at `krate` (its index for `texts`), whose root file is `root`, under
    /// `cfg` (its target's atoms and its features), crossing comments and strings by [`Vector`].
    pub fn scan<T: SourceTexts<'t> + ?Sized>(&mut self, texts: &T, krate: usize, root: &str, cfg: &Cfg<'_, '_>) -> Result<(), ScanError<'a>> {
        self.scan_by::<Vector, T>(texts, krate, root, cfg)
    }

    /// [`LibFeatures::scan`] crossing comments and strings by `J`: the one seam of the
    /// strategy, every `J` reading the same listing and the same errors (`jump_tests`).
    pub fn scan_by<J: Jump, T: SourceTexts<'t> + ?Sized>(&mut self, texts: &T, krate: usize, root: &str, cfg: &Cfg<'_, '_>) -> Result<(), ScanError<'a>> {
        let alloc = self.alloc;
        let file = joined(root, &[], alloc);
        let mut dir = file.clone_in(alloc);
        pop_dir(&mut dir);
        let dirs = Dirs { dir: dir.clone_in(alloc), path_dir: dir, file };
        let mut walk = Walk { texts, krate, cfg, out: self, alloc, jump: PhantomData::<J> };
        walk.file(dirs, 0)
    }

    /// What was read so far, as a listing in `alloc`.
    pub fn listed(&self, alloc: Alloc<'a>) -> Listed<'t, 'a> {
        let mut declared = AVec::from_slice_in(&self.declared, alloc);
        declared.sort_unstable();
        declared.dedup();
        let mut mentioned = AVec::from_slice_in(&self.mentioned, alloc);
        mentioned.sort_unstable();
        mentioned.dedup();
        mentioned.retain(|name| declared.binary_search(name).is_err());
        Listed { declared, mentioned, files: self.files }
    }
}

/// Where a module's files are, as blaze-scope's `walk` keeps them.
#[derive(Clone)]
struct Dirs<'a> {
    /// Where its `mod x;` finds `x.rs` and `x/mod.rs`.
    dir: AString<'a>,
    /// What its `#[path]` is relative to.
    path_dir: AString<'a>,
    /// The file being read: for errors, and what `include!` is relative to.
    file: AString<'a>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tok<'t> {
    Ident(&'t str),
    /// A string literal's contents as written (no escape is read).
    Str(&'t str),
    /// A number, a char, a byte or a lifetime.
    Lit,
    Punct(u8),
    Open(u8),
    Close(u8),
}

/// How the lexer crosses the inside of a comment or a string to the next byte that can end it
/// (rule 78: one seam, [`LibFeatures::scan_by`], each instantiation tested equal, `jump_tests`):
/// [`Scalar`] looks at every byte, [`Vector`] asks blaze-memchr, whose NEON, SSE2 or SWAR search
/// is chosen when blaze-memchr is compiled, so nothing is detected at run time (rule 75). Most of
/// a toolchain crate's bytes are doc comments, so crossing them is most of the walk's time.
pub trait Jump: Copy {
    /// The first index of `needle` in `hay`.
    fn byte(needle: u8, hay: &[u8]) -> Option<usize>;
    /// The first index of `a` or of `b` in `hay`.
    fn either(a: u8, b: u8, hay: &[u8]) -> Option<usize>;
}

/// A byte at a time: the reference [`Vector`] is held equal to, and the cost example's before.
#[derive(Clone, Copy)]
pub struct Scalar;

impl Jump for Scalar {
    fn byte(needle: u8, hay: &[u8]) -> Option<usize> {
        hay.iter().position(|c| *c == needle)
    }

    fn either(a: u8, b: u8, hay: &[u8]) -> Option<usize> {
        hay.iter().position(|c| *c == a || *c == b)
    }
}

/// blaze-memchr's search, sixteen or thirty-two bytes a step: the walk's.
#[derive(Clone, Copy)]
pub struct Vector;

impl Jump for Vector {
    #[inline]
    fn byte(needle: u8, hay: &[u8]) -> Option<usize> {
        blaze_memchr::memchr(needle, hay)
    }

    #[inline]
    fn either(a: u8, b: u8, hay: &[u8]) -> Option<usize> {
        blaze_memchr::memchr2(a, b, hay)
    }
}

/// Tokens of one text, comments skipped, crossing comments and strings by `J`. Copied to look
/// ahead.
#[derive(Clone, Copy)]
struct Lex<'t, J: Jump> {
    text: &'t str,
    at: usize,
    jump: PhantomData<J>,
}

fn ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic() || b >= 0x80
}

fn ident_byte(b: u8) -> bool {
    ident_start(b) || b.is_ascii_digit()
}

/// The closing quote of the string whose contents start at `at`: from one `\` or `"` to the
/// next, a `\` taking the byte after it.
fn quoted<J: Jump>(b: &[u8], mut at: usize) -> Option<usize> {
    while at < b.len() {
        at += J::either(b'\\', b'"', &b[at..])?;
        if b[at] == b'"' {
            return Some(at);
        }
        at += 2;
    }
    None
}

/// Past the char literal or the lifetime whose quote is at `at`.
fn quote_or_lifetime<J: Jump>(b: &[u8], at: usize) -> Option<usize> {
    let n = at + 1;
    let c = *b.get(n)?;
    if c == b'\\' {
        let mut e = n + 2;
        if e < b.len() {
            e += J::byte(b'\'', &b[e..]).unwrap_or(b.len() - e);
        }
        return (e < b.len()).then_some(e + 1);
    }
    let width = match c {
        0..0x80 => 1,
        0xc0..0xe0 => 2,
        0xe0..0xf0 => 3,
        _ => 4,
    };
    if b.get(n + width) == Some(&b'\'') {
        return Some(n + width + 1);
    }
    let mut e = n;
    while e < b.len() && ident_byte(b[e]) {
        e += 1;
    }
    Some(e)
}

impl<'t, J: Jump> Lex<'t, J> {
    /// The next token and its byte, `None` at the end; `Err(at)` where text does not lex.
    fn next(&mut self) -> Result<Option<(usize, Tok<'t>)>, usize> {
        let b = self.text.as_bytes();
        let text = self.text;
        let mut at = self.at;
        loop {
            while at < b.len() && b[at].is_ascii_whitespace() {
                at += 1;
            }
            if b[at..].starts_with(b"//") {
                at += J::byte(b'\n', &b[at..]).unwrap_or(b.len() - at);
            } else if b[at..].starts_with(b"/*") {
                let start = at;
                let mut depth = 0usize;
                loop {
                    if b[at..].starts_with(b"/*") {
                        depth += 1;
                        at += 2;
                    } else if b[at..].starts_with(b"*/") {
                        depth -= 1;
                        at += 2;
                        if depth == 0 {
                            break;
                        }
                    } else if at < b.len() {
                        // Only a `/` or a `*` can open or close a comment: to the next one.
                        at += 1 + J::either(b'/', b'*', &b[at + 1..]).unwrap_or(b.len() - at - 1);
                    } else {
                        return Err(start);
                    }
                }
            } else {
                break;
            }
        }
        if at >= b.len() {
            self.at = at;
            return Ok(None);
        }
        let start = at;
        let c = b[at];
        let tok = match c {
            b'"' => {
                let end = quoted::<J>(b, at + 1).ok_or(start)?;
                at = end + 1;
                Tok::Str(&text[start + 1..end])
            }
            b'\'' => {
                at = quote_or_lifetime::<J>(b, at).ok_or(start)?;
                Tok::Lit
            }
            b'(' | b'[' | b'{' => {
                at += 1;
                Tok::Open(c)
            }
            b')' | b']' | b'}' => {
                at += 1;
                Tok::Close(c)
            }
            b'0'..=b'9' => {
                at += 1;
                while at < b.len() && ident_byte(b[at]) {
                    at += 1;
                }
                Tok::Lit
            }
            c if ident_start(c) => {
                // A prefixed literal (`b".."`, `c".."`, `b'.'`, `r#".."#`, `br".."`), a raw
                // identifier (`r#name`), else an identifier.
                let mut p = at;
                if c == b'b' || c == b'c' {
                    p += 1;
                }
                let mut hashes = 0;
                let mut q = p + 1;
                if b.get(p) == Some(&b'r') {
                    while b.get(q) == Some(&b'#') {
                        hashes += 1;
                        q += 1;
                    }
                }
                if b.get(p) == Some(&b'r') && b.get(q) == Some(&b'"') {
                    let body = q + 1;
                    let mut e = body;
                    loop {
                        // To the next `"`, then whether the hashes follow it.
                        e += J::byte(b'"', &b[e..]).ok_or(start)?;
                        if b.len() >= e + 1 + hashes && b[e + 1..e + 1 + hashes].iter().all(|h| *h == b'#') {
                            break;
                        }
                        e += 1;
                    }
                    at = e + 1 + hashes;
                    Tok::Str(&text[body..e])
                } else if p == at && b.get(p) == Some(&b'r') && hashes == 1 && b.get(q).is_some_and(|x| ident_start(*x)) {
                    let s = q;
                    while q < b.len() && ident_byte(b[q]) {
                        q += 1;
                    }
                    at = q;
                    Tok::Ident(&text[s..q])
                } else if p > at && b.get(p) == Some(&b'"') {
                    let end = quoted::<J>(b, p + 1).ok_or(start)?;
                    at = end + 1;
                    Tok::Str(&text[p + 1..end])
                } else if c == b'b' && b.get(at + 1) == Some(&b'\'') {
                    at = quote_or_lifetime::<J>(b, at + 1).ok_or(start)?;
                    Tok::Lit
                } else {
                    while at < b.len() && ident_byte(b[at]) {
                        at += 1;
                    }
                    Tok::Ident(&text[start..at])
                }
            }
            _ => {
                at += 1;
                Tok::Punct(c)
            }
        };
        self.at = at;
        Ok(Some((start, tok)))
    }
}

/// How an attribute's meta item ended.
enum End {
    Comma,
    Close,
}

/// What an item's attributes say besides the features (which go to `pending`).
struct Attrs<'t> {
    on: bool,
    path: Option<&'t str>,
}

/// Whether the items go on.
enum Step {
    More,
    End,
}

struct Walk<'w, 't, 'a, 'c, 'd, T: ?Sized, J: Jump> {
    texts: &'w T,
    krate: usize,
    cfg: &'w Cfg<'c, 'd>,
    out: &'w mut LibFeatures<'t, 'a>,
    alloc: Alloc<'a>,
    jump: PhantomData<J>,
}

type R<'a, T> = Result<T, ScanError<'a>>;

impl<'w, 't, 'a, 'c, 'd, T: SourceTexts<'t> + ?Sized, J: Jump> Walk<'w, 't, 'a, 'c, 'd, T, J> {
    fn file(&mut self, dirs: Dirs<'a>, depth: usize) -> R<'a, ()> {
        self.out.files += 1;
        if self.out.files > MOST_FILES {
            return Err(ScanError::TooManyFiles);
        }
        let Some(text) = self.texts.read(self.krate, &dirs.file) else {
            return Err(ScanError::Missing { path: dirs.file });
        };
        let at = if text.starts_with('\u{feff}') { 3 } else { 0 };
        let mut lex: Lex<'t, J> = Lex { text, at, jump: PhantomData };
        self.items(&mut lex, &dirs, true, depth)
    }

    fn next(&self, lex: &mut Lex<'t, J>, d: &Dirs<'a>) -> R<'a, Option<(usize, Tok<'t>)>> {
        lex.next().map_err(|at| ScanError::Text { file: d.file.clone_in(self.alloc), at })
    }

    fn want(&self, lex: &mut Lex<'t, J>, d: &Dirs<'a>) -> R<'a, (usize, Tok<'t>)> {
        match self.next(lex, d)? {
            Some(it) => Ok(it),
            None => Err(ScanError::Text { file: d.file.clone_in(self.alloc), at: lex.at }),
        }
    }

    fn peek(&self, lex: &Lex<'t, J>, d: &Dirs<'a>) -> R<'a, Option<Tok<'t>>> {
        let mut l = *lex;
        Ok(self.next(&mut l, d)?.map(|(_, tok)| tok))
    }

    /// Consumes the next token when it is `tok`.
    fn eat(&self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, tok: Tok<'t>) -> R<'a, bool> {
        let mut l = *lex;
        if self.next(&mut l, d)?.map(|(_, t)| t) == Some(tok) {
            *lex = l;
            return Ok(true);
        }
        Ok(false)
    }

    fn deeper(&self, d: &Dirs<'a>, at: usize, depth: usize) -> R<'a, usize> {
        if depth >= MOST_DEPTH {
            return Err(ScanError::TooDeep { file: d.file.clone_in(self.alloc), at });
        }
        Ok(depth + 1)
    }

    /// Past the group whose opener was just read, to its closer (returned: its byte). With
    /// `mention`, `feature = "x"` inside is a mention and `mod x;` is not followed.
    fn group(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, mention: bool) -> R<'a, usize> {
        let mut depth = 1usize;
        let mut last = [Tok::Lit; 2];
        loop {
            let (at, tok) = self.want(lex, d)?;
            match tok {
                Tok::Open(_) => depth += 1,
                Tok::Close(_) => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(at);
                    }
                }
                Tok::Str(name) if mention && last == [Tok::Ident("feature"), Tok::Punct(b'=')] => self.out.mentioned.push(name),
                Tok::Punct(b';') if mention && matches!(last, [Tok::Ident("mod"), Tok::Ident(_)]) => {
                    return Err(ScanError::Unfollowed { file: d.file.clone_in(self.alloc), at });
                }
                _ => {}
            }
            last = [last[1], tok];
        }
    }

    /// The pending features from `mark` on: declared when `on`, else dropped.
    fn settle(&mut self, mark: usize, on: bool) {
        if on {
            for at in mark..self.out.pending.len() {
                let name = self.out.pending[at];
                self.out.declared.push(name);
            }
        }
        self.out.pending.truncate(mark);
    }

    /// The rest of a meta item, past the `,` or the closer that ends it.
    fn rest_of_meta(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>) -> R<'a, End> {
        loop {
            match self.want(lex, d)?.1 {
                Tok::Open(_) => {
                    self.group(lex, d, false)?;
                }
                Tok::Punct(b',') => return Ok(End::Comma),
                Tok::Close(_) => return Ok(End::Close),
                _ => {}
            }
        }
    }

    /// One meta item of an attribute (after `#[`, or in `cfg_attr(..)` after its predicate).
    fn meta(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, st: &mut Attrs<'t>) -> R<'a, End> {
        let name = match self.want(lex, d)?.1 {
            Tok::Ident(name) => name,
            Tok::Close(_) => return Ok(End::Close),
            Tok::Punct(b',') => return Ok(End::Comma),
            Tok::Open(_) => {
                self.group(lex, d, false)?;
                return self.rest_of_meta(lex, d);
            }
            _ => return self.rest_of_meta(lex, d),
        };
        let (open, tok) = self.want(lex, d)?;
        match tok {
            Tok::Punct(b',') => Ok(End::Comma),
            Tok::Close(_) => Ok(End::Close),
            // A path (`rustfmt::skip`, `diagnostic::on_unimplemented`): no builtin.
            Tok::Punct(b':') => self.rest_of_meta(lex, d),
            Tok::Punct(b'=') => {
                if name == "path" {
                    let (at, value) = self.want(lex, d)?;
                    match value {
                        Tok::Str(path) => st.path = Some(path),
                        _ => return Err(ScanError::Unfollowed { file: d.file.clone_in(self.alloc), at }),
                    }
                }
                self.rest_of_meta(lex, d)
            }
            Tok::Open(b'(') => {
                match name {
                    "cfg" => {
                        let close = self.group(lex, d, false)?;
                        match self.cfg.eval(&lex.text[open + 1..close]) {
                            Some(on) => st.on &= on,
                            None => return Err(ScanError::Cfg { file: d.file.clone_in(self.alloc), at: open }),
                        }
                    }
                    "cfg_attr" => self.cfg_attr(lex, d, open, st)?,
                    name if STABILITY.contains(&name) => {
                        let mut depth = 1usize;
                        let mut last = [Tok::Lit; 2];
                        loop {
                            let tok = self.want(lex, d)?.1;
                            match tok {
                                Tok::Open(_) => depth += 1,
                                Tok::Close(_) => {
                                    depth -= 1;
                                    if depth == 0 {
                                        break;
                                    }
                                }
                                Tok::Str(feature) if depth == 1 && last == [Tok::Ident("feature"), Tok::Punct(b'=')] => self.out.pending.push(feature),
                                _ => {}
                            }
                            last = [last[1], tok];
                        }
                    }
                    _ => {
                        self.group(lex, d, false)?;
                    }
                }
                self.rest_of_meta(lex, d)
            }
            Tok::Open(_) => {
                self.group(lex, d, false)?;
                self.rest_of_meta(lex, d)
            }
            _ => self.rest_of_meta(lex, d),
        }
    }

    /// `cfg_attr(pred, attr, ..)` after its `(` (at `open`): the attributes when `pred` holds.
    fn cfg_attr(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, open: usize, st: &mut Attrs<'t>) -> R<'a, ()> {
        let mut depth = 0usize;
        let comma = loop {
            let (at, tok) = self.want(lex, d)?;
            match tok {
                Tok::Open(_) => depth += 1,
                Tok::Close(_) if depth == 0 => return Err(ScanError::Text { file: d.file.clone_in(self.alloc), at }),
                Tok::Close(_) => depth -= 1,
                Tok::Punct(b',') if depth == 0 => break at,
                _ => {}
            }
        };
        match self.cfg.eval(&lex.text[open + 1..comma]) {
            Some(true) => loop {
                if let End::Close = self.meta(lex, d, st)? {
                    return Ok(());
                }
            },
            Some(false) => {
                self.group(lex, d, false)?;
                Ok(())
            }
            None => Err(ScanError::Cfg { file: d.file.clone_in(self.alloc), at: open }),
        }
    }

    /// The attributes `#[..]` (or, `inner`, `#![..]`) next, read into `st`.
    fn attributes(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, inner: bool, st: &mut Attrs<'t>) -> R<'a, ()> {
        loop {
            let mut l = *lex;
            if self.next(&mut l, d)?.map(|(_, t)| t) != Some(Tok::Punct(b'#')) {
                return Ok(());
            }
            if inner && self.next(&mut l, d)?.map(|(_, t)| t) != Some(Tok::Punct(b'!')) {
                return Ok(());
            }
            let Some((at, Tok::Open(b'['))) = self.next(&mut l, d)? else { return Ok(()) };
            *lex = l;
            if let End::Comma = self.meta(lex, d, st)? {
                return Err(ScanError::Text { file: d.file.clone_in(self.alloc), at });
            }
        }
    }

    /// The items of a module or a block up to its closer (consumed) or the file's end.
    fn items(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, on: bool, depth: usize) -> R<'a, ()> {
        let mark = self.out.pending.len();
        let mut st = Attrs { on: true, path: None };
        self.attributes(lex, d, true, &mut st)?;
        let on = on && st.on;
        self.settle(mark, on);
        loop {
            let mark = self.out.pending.len();
            let mut st = Attrs { on: true, path: None };
            self.attributes(lex, d, false, &mut st)?;
            let item_on = on && st.on;
            self.settle(mark, item_on);
            if let Step::End = self.item(lex, d, item_on, st.path, depth)? {
                return Ok(());
            }
        }
    }

    /// One item, its attributes read.
    fn item(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, on: bool, path: Option<&'t str>, depth: usize) -> R<'a, Step> {
        let Some((mut at, mut tok)) = self.next(lex, d)? else { return Ok(Step::End) };
        loop {
            match tok {
                Tok::Ident("pub") => {
                    if self.eat(lex, d, Tok::Open(b'('))? {
                        self.group(lex, d, false)?;
                    }
                }
                Tok::Ident("unsafe" | "async" | "default" | "auto" | "safe") => {}
                Tok::Ident("extern") => {
                    if let Some(Tok::Str(_)) = self.peek(lex, d)? {
                        self.want(lex, d)?;
                    }
                    match self.peek(lex, d)? {
                        Some(Tok::Open(b'{')) => {
                            let (open, _) = self.want(lex, d)?;
                            let depth = self.deeper(d, open, depth)?;
                            self.items(lex, d, on, depth)?;
                            return Ok(Step::More);
                        }
                        Some(Tok::Ident("crate")) => return self.rest(lex, d, on),
                        _ => {}
                    }
                }
                Tok::Ident("const") => {
                    if !matches!(self.peek(lex, d)?, Some(Tok::Ident("fn" | "unsafe" | "async" | "extern"))) {
                        return self.rest(lex, d, on);
                    }
                }
                _ => break,
            }
            (at, tok) = self.want(lex, d)?;
        }
        match tok {
            Tok::Close(_) => Ok(Step::End),
            Tok::Punct(b';') => Ok(Step::More),
            Tok::Ident("mod") => {
                let Tok::Ident(name) = self.want(lex, d)?.1 else {
                    return self.rest(lex, d, on);
                };
                let (open, next) = self.want(lex, d)?;
                match next {
                    Tok::Punct(b';') => {
                        if on {
                            self.file_mod(d, name, path, at, depth)?;
                        }
                        Ok(Step::More)
                    }
                    Tok::Open(b'{') => {
                        let child = match path {
                            Some(p) => joined(&d.path_dir, &[p], self.alloc),
                            None => joined(&d.dir, &[name], self.alloc),
                        };
                        let dirs = Dirs { dir: child.clone_in(self.alloc), path_dir: child, file: d.file.clone_in(self.alloc) };
                        let depth = self.deeper(d, open, depth)?;
                        self.items(lex, &dirs, on, depth)?;
                        Ok(Step::More)
                    }
                    _ => Err(ScanError::Text { file: d.file.clone_in(self.alloc), at: open }),
                }
            }
            Tok::Ident("impl" | "trait") => loop {
                let (open, tok) = self.want(lex, d)?;
                match tok {
                    Tok::Open(b'{') => {
                        let depth = self.deeper(d, open, depth)?;
                        self.items(lex, d, on, depth)?;
                        return Ok(Step::More);
                    }
                    Tok::Open(_) => {
                        self.group(lex, d, on)?;
                    }
                    Tok::Punct(b';') => return Ok(Step::More),
                    Tok::Close(_) => return Ok(Step::End),
                    _ => {}
                }
            },
            Tok::Ident("struct" | "union" | "enum") => loop {
                let (open, tok) = self.want(lex, d)?;
                match tok {
                    Tok::Open(b'{') => {
                        let depth = self.deeper(d, open, depth)?;
                        self.list(lex, d, on, depth)?;
                        return Ok(Step::More);
                    }
                    Tok::Open(b'(') => {
                        let depth = self.deeper(d, open, depth)?;
                        self.list(lex, d, on, depth)?;
                    }
                    Tok::Open(_) => {
                        self.group(lex, d, on)?;
                    }
                    Tok::Punct(b';') => return Ok(Step::More),
                    Tok::Close(_) => return Ok(Step::End),
                    _ => {}
                }
            },
            Tok::Ident("fn") => loop {
                match self.want(lex, d)?.1 {
                    Tok::Open(b'{') => {
                        self.group(lex, d, on)?;
                        return Ok(Step::More);
                    }
                    Tok::Open(_) => {
                        self.group(lex, d, on)?;
                    }
                    Tok::Punct(b';') => return Ok(Step::More),
                    Tok::Close(_) => return Ok(Step::End),
                    _ => {}
                }
            },
            Tok::Ident("macro") => loop {
                match self.want(lex, d)?.1 {
                    Tok::Open(b'{') => {
                        self.group(lex, d, on)?;
                        return Ok(Step::More);
                    }
                    Tok::Open(_) => {
                        self.group(lex, d, on)?;
                    }
                    Tok::Punct(b';') => return Ok(Step::More),
                    Tok::Close(_) => return Ok(Step::End),
                    _ => {}
                }
            },
            Tok::Ident(first) => {
                // A macro call `a::b!(..)`, `name! { .. }` (`macro_rules! name { .. }` among
                // them), else another item.
                let mut l = *lex;
                let mut name = first;
                let call = loop {
                    match self.next(&mut l, d)?.map(|(_, t)| t) {
                        Some(Tok::Punct(b'!')) => break true,
                        Some(Tok::Punct(b':')) => {
                            if self.next(&mut l, d)?.map(|(_, t)| t) != Some(Tok::Punct(b':')) {
                                break false;
                            }
                            match self.next(&mut l, d)?.map(|(_, t)| t) {
                                Some(Tok::Ident(segment)) => name = segment,
                                _ => break false,
                            }
                        }
                        _ => break false,
                    }
                };
                if !call {
                    return self.rest(lex, d, on);
                }
                *lex = l;
                if name == "macro_rules" {
                    self.want(lex, d)?;
                }
                let (open, tok) = self.want(lex, d)?;
                match (name, tok) {
                    ("cfg_select", Tok::Open(b'{')) => {
                        let depth = self.deeper(d, open, depth)?;
                        self.cfg_select(lex, d, on, depth)?;
                    }
                    ("cfg_if", Tok::Open(b'{')) => {
                        let depth = self.deeper(d, open, depth)?;
                        self.cfg_if(lex, d, on, depth)?;
                    }
                    ("include", Tok::Open(b'(')) if on => self.include(lex, d, open, depth)?,
                    (_, Tok::Open(_)) => {
                        self.group(lex, d, on)?;
                    }
                    _ => return Err(ScanError::Text { file: d.file.clone_in(self.alloc), at: open }),
                }
                self.eat(lex, d, Tok::Punct(b';'))?;
                Ok(Step::More)
            }
            Tok::Open(_) => {
                self.group(lex, d, on)?;
                self.rest(lex, d, on)
            }
            _ => self.rest(lex, d, on),
        }
    }

    /// The rest of an item of no body the walk reads: past its `;`, or past a `{..}` that ends
    /// it (`use a::{b, c}` leaves its `;` an empty item).
    fn rest(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, on: bool) -> R<'a, Step> {
        loop {
            let Some((_, tok)) = self.next(lex, d)? else { return Ok(Step::End) };
            match tok {
                Tok::Punct(b';') => return Ok(Step::More),
                Tok::Open(b'{') => {
                    self.group(lex, d, on)?;
                    return Ok(Step::More);
                }
                Tok::Open(_) => {
                    self.group(lex, d, on)?;
                }
                Tok::Close(_) => return Ok(Step::End),
                _ => {}
            }
        }
    }

    /// Fields or variants up to the list's closer, each with its own attributes.
    fn list(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, on: bool, depth: usize) -> R<'a, ()> {
        let mut stack = [(false, false); MOST_DEPTH + 1];
        let mut sp = 0usize;
        let mut cur_on = on;
        let mut cur_depth = depth;
        'elem: loop {
            let mark = self.out.pending.len();
            let mut st = Attrs { on: true, path: None };
            self.attributes(lex, d, false, &mut st)?;
            let mut element_on = cur_on && st.on;
            self.settle(mark, element_on);
            loop {
                let (open, tok) = self.want(lex, d)?;
                match tok {
                    Tok::Close(_) => {
                        if sp == 0 {
                            return Ok(());
                        }
                        sp -= 1;
                        let &(prev_on, prev_elem) = stack.get(sp).unwrap_or(&(false, false));
                        cur_on = prev_on;
                        element_on = prev_elem;
                        cur_depth -= 1;
                    }
                    Tok::Punct(b',') => continue 'elem,
                    Tok::Open(b'{' | b'(') => {
                        let next_depth = self.deeper(d, open, cur_depth)?;
                        if let Some(slot) = stack.get_mut(sp) {
                            *slot = (cur_on, element_on);
                        }
                        sp += 1;
                        cur_on = element_on;
                        cur_depth = next_depth;
                        continue 'elem;
                    }
                    Tok::Open(_) => {
                        self.group(lex, d, element_on)?;
                    }
                    _ => {}
                }
            }
        }
    }

    /// `cfg_select! { pred => { items } .. _ => { items } }` after its `{`: the first arm whose
    /// predicate holds.
    fn cfg_select(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, on: bool, depth: usize) -> R<'a, ()> {
        let mut chosen = false;
        loop {
            let (start, tok) = self.want(lex, d)?;
            if let Tok::Close(_) = tok {
                return Ok(());
            }
            let mut tok = tok;
            let mut at = start;
            let arrow = loop {
                match tok {
                    Tok::Punct(b'=') if self.peek(lex, d)? == Some(Tok::Punct(b'>')) => {
                        self.want(lex, d)?;
                        break at;
                    }
                    Tok::Open(_) => {
                        self.group(lex, d, false)?;
                    }
                    Tok::Close(_) => return Err(ScanError::Text { file: d.file.clone_in(self.alloc), at }),
                    _ => {}
                }
                (at, tok) = self.want(lex, d)?;
            };
            let predicate = lex.text[start..arrow].trim();
            let holds = match predicate {
                "_" => true,
                predicate => match self.cfg.eval(predicate) {
                    Some(holds) => holds,
                    None => return Err(ScanError::Cfg { file: d.file.clone_in(self.alloc), at: start }),
                },
            };
            let (open, body) = self.want(lex, d)?;
            if body != Tok::Open(b'{') {
                return Err(ScanError::Unfollowed { file: d.file.clone_in(self.alloc), at: open });
            }
            if on && holds && !chosen {
                chosen = true;
                let depth = self.deeper(d, open, depth)?;
                self.items(lex, d, true, depth)?;
            } else {
                self.group(lex, d, false)?;
            }
            self.eat(lex, d, Tok::Punct(b','))?;
        }
    }

    /// `cfg_if! { if #[cfg(p)] { items } else if #[cfg(q)] { items } else { items } }` after its
    /// `{` (the cfg-if crate's grammar; std's `backtrace` and `sys` use it): in each chain, the
    /// first branch whose predicate holds. Any other shape could hide a module: `Unfollowed`.
    fn cfg_if(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, on: bool, depth: usize) -> R<'a, ()> {
        loop {
            let (at, tok) = self.want(lex, d)?;
            match tok {
                Tok::Close(_) => return Ok(()),
                Tok::Ident("if") => {}
                _ => return Err(ScanError::Unfollowed { file: d.file.clone_in(self.alloc), at }),
            }
            let mut chosen = false;
            loop {
                let take = self.cfg_if_predicate(lex, d)? && !chosen;
                self.cfg_if_branch(lex, d, on && take, depth)?;
                chosen |= take;
                if !self.eat(lex, d, Tok::Ident("else"))? {
                    break;
                }
                if !self.eat(lex, d, Tok::Ident("if"))? {
                    self.cfg_if_branch(lex, d, on && !chosen, depth)?;
                    break;
                }
            }
        }
    }

    /// A `cfg_if!` branch's `#[cfg(predicate)]`, read and evaluated.
    fn cfg_if_predicate(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>) -> R<'a, bool> {
        for want in [Tok::Punct(b'#'), Tok::Open(b'['), Tok::Ident("cfg")] {
            let (at, got) = self.want(lex, d)?;
            if got != want {
                return Err(ScanError::Unfollowed { file: d.file.clone_in(self.alloc), at });
            }
        }
        let (open, paren) = self.want(lex, d)?;
        if paren != Tok::Open(b'(') {
            return Err(ScanError::Unfollowed { file: d.file.clone_in(self.alloc), at: open });
        }
        let close = self.group(lex, d, false)?;
        let (end, bracket) = self.want(lex, d)?;
        if !matches!(bracket, Tok::Close(_)) {
            return Err(ScanError::Unfollowed { file: d.file.clone_in(self.alloc), at: end });
        }
        match self.cfg.eval(lex.text[open + 1..close].trim()) {
            Some(holds) => Ok(holds),
            None => Err(ScanError::Cfg { file: d.file.clone_in(self.alloc), at: open }),
        }
    }

    /// A `cfg_if!` branch's `{ items }`: walked when `on`, else passed over.
    fn cfg_if_branch(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, on: bool, depth: usize) -> R<'a, ()> {
        let (open, body) = self.want(lex, d)?;
        if body != Tok::Open(b'{') {
            return Err(ScanError::Unfollowed { file: d.file.clone_in(self.alloc), at: open });
        }
        if on {
            let depth = self.deeper(d, open, depth)?;
            self.items(lex, d, true, depth)
        } else {
            self.group(lex, d, false).map(|_| ())
        }
    }

    /// `include!("file")` after its `(` at `open`: the file's items, in this module.
    fn include(&mut self, lex: &mut Lex<'t, J>, d: &Dirs<'a>, open: usize, depth: usize) -> R<'a, ()> {
        let (Tok::Str(file), Tok::Close(_)) = (self.want(lex, d)?.1, self.want(lex, d)?.1) else {
            return Err(ScanError::Unfollowed { file: d.file.clone_in(self.alloc), at: open });
        };
        let mut from = d.file.clone_in(self.alloc);
        pop_dir(&mut from);
        let target = joined(&from, &[file], self.alloc);
        let dirs = Dirs { dir: d.dir.clone_in(self.alloc), path_dir: d.path_dir.clone_in(self.alloc), file: target };
        let depth = self.deeper(d, open, depth)?;
        self.file(dirs, depth)
    }

    /// `mod name;` (on, at `at`): its file, by `#[path]` or the Reference's rules.
    fn file_mod(&mut self, d: &Dirs<'a>, name: &str, path: Option<&str>, at: usize, depth: usize) -> R<'a, ()> {
        let alloc = self.alloc;
        let dirs = if let Some(p) = path {
            let target = joined(&d.path_dir, &[p], alloc);
            let mut parent = target.clone_in(alloc);
            pop_dir(&mut parent);
            Dirs { dir: parent.clone_in(alloc), path_dir: parent, file: target }
        } else {
            let mut file_name = AString::new_in(alloc);
            file_name.push_str(name);
            file_name.push_str(".rs");
            let a = joined(&d.dir, &[&file_name], alloc);
            let sub = joined(&d.dir, &[name], alloc);
            if self.texts.read(self.krate, &a).is_some() {
                Dirs { dir: sub, path_dir: d.dir.clone_in(alloc), file: a }
            } else {
                let b = joined(&d.dir, &[name, "mod.rs"], alloc);
                Dirs { dir: sub.clone_in(alloc), path_dir: sub, file: b }
            }
        };
        let depth = self.deeper(d, at, depth)?;
        self.file(dirs, depth)
    }
}

#[cfg(test)]
mod tests;

/// [`Vector`] held equal to [`Scalar`] (rule 78): token by token on drawn texts, with planted
/// searches each caught, and listing by listing on drawn crates and on the tree's `library/`.
#[cfg(test)]
mod jump_tests {
    use core::{fmt, marker::PhantomData};
    use std::println;
    use std::string::String as StdString;

    use blaze_own::{AString, AVec, Alloc, testing::heap};
    use blaze_sim::{Broken, Config, Tape, check, controls};

    use super::{Jump, Lex, LibFeatures, LibFeaturesOpts, Listed, ScanError, Scalar, Tok, Vector};
    use crate::resolve::SourceTexts;
    use crate::task::Cfg;

    /// The bytes that start, end or escape a comment, a string, a char or a lifetime, and the
    /// forms they make together; between them a drawn run of filler, so a search crosses a
    /// long stretch (blaze-memchr's vector loop runs on 16 bytes and more) as well as a short.
    const MARKS: &[&str] = &[
        "/", "*", "\"", "\\", "'", "#", "r", "b", "c", "\n", " ", "{", "}", "é", "//", "/*", "*/", "r#\"", "\"#", "br\"", "b'", "'a", "'\\''", "\\\"",
    ];

    /// A text of drawn marks and filler runs, up to about 2,500 bytes.
    fn drawn_text<'a>(tape: &Tape, alloc: Alloc<'a>) -> AString<'a> {
        let mut text = AString::new_in(alloc);
        let pieces = tape.int_in(0..=24usize, "pieces");
        for _ in 0..pieces {
            if tape.weighted(&[1, 2], "filler or mark") == 0 {
                let run = tape.int_in(0..=100usize, "filler bytes");
                for _ in 0..run {
                    text.push('x');
                }
            } else {
                text.push_str(tape.one_of(MARKS, "mark"));
            }
        }
        text
    }

    type Step<'t> = Result<Option<(usize, Tok<'t>)>, usize>;

    /// Each step of lexing `text` by `J`, to the end or the first error.
    fn steps<'t, 'a, J: Jump>(text: &'t str, alloc: Alloc<'a>) -> AVec<'a, Step<'t>> {
        let mut lex: Lex<'t, J> = Lex { text, at: 0, jump: PhantomData };
        let mut out = AVec::new_in(alloc);
        // A token takes a byte at least, so a step a byte and the end bound it (rule 18).
        for _ in 0..=text.len() {
            let step = lex.next();
            let more = matches!(step, Ok(Some(_)));
            out.push(step);
            if !more {
                break;
            }
        }
        out
    }

    /// blaze-memchr's search blind to the hay's last byte: a vector tail that is not read.
    #[derive(Clone, Copy)]
    struct MissesLast;

    impl Jump for MissesLast {
        fn byte(needle: u8, hay: &[u8]) -> Option<usize> {
            Vector::byte(needle, &hay[..hay.len().saturating_sub(1)])
        }

        fn either(a: u8, b: u8, hay: &[u8]) -> Option<usize> {
            Vector::either(a, b, &hay[..hay.len().saturating_sub(1)])
        }
    }

    /// A two-needle search that looks for its second needle only.
    #[derive(Clone, Copy)]
    struct SecondOnly;

    impl Jump for SecondOnly {
        fn byte(needle: u8, hay: &[u8]) -> Option<usize> {
            Vector::byte(needle, hay)
        }

        fn either(_a: u8, b: u8, hay: &[u8]) -> Option<usize> {
            Vector::byte(b, hay)
        }
    }

    #[derive(Debug)]
    enum LexBroken {
        /// The two lexings part at step `step` (or one ends there).
        Parts { step: usize, len: usize },
    }

    impl fmt::Display for LexBroken {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                LexBroken::Parts { step, len } => write!(f, "a {len}-byte text lexes otherwise by the vector search at step {step}"),
            }
        }
    }

    blaze_sim::plants! {
        /// Searches that do not find what a byte-at-a-time look finds.
        enum JumpPlant breaks LexBroken {
            /// blaze-memchr, the walk's.
            None,
            /// The hay's last byte is not searched.
            MissesLast => LexBroken::Parts { .. },
            /// `either` finds its second needle only.
            SecondOnly => LexBroken::Parts { .. },
        }
    }

    fn same_tokens(tape: &Tape, plant: JumpPlant) -> Result<(), Broken<LexBroken>> {
        let alloc = heap().alloc();
        let text = drawn_text(tape, alloc);
        let want = steps::<Scalar>(text.as_str(), alloc);
        let got = match plant {
            JumpPlant::None => steps::<Vector>(text.as_str(), alloc),
            JumpPlant::MissesLast => steps::<MissesLast>(text.as_str(), alloc),
            JumpPlant::SecondOnly => steps::<SecondOnly>(text.as_str(), alloc),
        };
        let parted = want.iter().zip(got.iter()).position(|(w, g)| w != g);
        match parted {
            Some(step) => Err(Broken::Oracle(LexBroken::Parts { step, len: text.len() })),
            None if want.len() != got.len() => Err(Broken::Oracle(LexBroken::Parts { step: want.len().min(got.len()), len: text.len() })),
            None => Ok(()),
        }
    }

    /// Every step of a drawn text lexes the same by the vector search as byte by byte (tokens,
    /// their bytes, the end, the byte of an error), and each planted search is caught.
    #[test]
    fn vector_lexes_as_scalar() {
        controls(Config::cases(1024), same_tokens);
    }

    /// Items with stability attributes in places that declare, mention or hide them, each with
    /// the bytes that start comments and strings around it, and two that do not close.
    const ITEMS: &[&str] = &[
        "#[stable(feature = \"a\", since = \"1\")]\npub fn a() {}\n",
        "#[unstable(feature = \"b\", issue = \"none\")]\npub struct B {\n    #[stable(feature = \"field\", since = \"1\")]\n    pub f: u8,\n}\n",
        "/// a doc with #[unstable(feature = \"in_doc\")] and a \" and a '\n",
        "/* a block /* nested */ #[stable(feature = \"in_block\")] */\n",
        "// a line #[stable(feature = \"in_line\")]\n",
        "const S: &str = \"an escape \\\" #[stable(feature = \\\"in_string\\\")]\";\n",
        "const R: &str = r#\"raw \" #[unstable(feature = \"in_raw\")]\"#;\n",
        "const C: char = '\\'';\nconst D: u8 = b'\"';\n",
        "fn l<'a>(x: &'a u8) -> u8 { *x }\n",
        "macro_rules! m { () => { #[unstable(feature = \"mentioned\")] fn y() {} }; }\n",
        "mod inner {\n    #[stable(feature = \"inner\", since = \"1\")]\n    pub fn i() {}\n}\n",
        "impl S {\n    #[rustc_const_unstable(feature = \"const_s\", issue = \"none\")]\n    pub const fn s() {}\n}\n",
        "\"an unclosed string\n",
        "/* an unclosed comment\n",
    ];

    /// One file, the crate's root.
    struct One<'t>(&'t str);

    const ROOT: &str = "/0/c/src/lib.rs";

    impl<'t> SourceTexts<'t> for One<'t> {
        fn read(&self, _krate: usize, path: &str) -> Option<&'t str> {
            (path == ROOT).then_some(self.0)
        }
    }

    /// What a scan came to, comparable: the listing, or the error's kind and byte.
    #[derive(PartialEq, Debug)]
    enum Seen<'t, 'a> {
        Listed { declared: AVec<'a, &'t str>, mentioned: AVec<'a, &'t str>, files: usize },
        Refused { kind: u8, at: usize },
    }

    fn seen<'t, 'a>(scanned: Result<Listed<'t, 'a>, ScanError<'a>>) -> Seen<'t, 'a> {
        match scanned {
            Ok(listed) => Seen::Listed { declared: listed.declared, mentioned: listed.mentioned, files: listed.files },
            Err(why) => {
                let (kind, at) = match why {
                    ScanError::Missing { .. } => (0, 0),
                    ScanError::Cfg { at, .. } => (1, at),
                    ScanError::Text { at, .. } => (2, at),
                    ScanError::Unfollowed { at, .. } => (3, at),
                    ScanError::TooDeep { at, .. } => (4, at),
                    ScanError::TooManyFiles => (5, 0),
                };
                Seen::Refused { kind, at }
            }
        }
    }

    fn scan_one<'t, 'a, J: Jump>(text: &'t str, alloc: Alloc<'a>) -> Seen<'t, 'a> {
        let cfg = Cfg::from_atoms(&[], alloc);
        let mut features = LibFeatures::new(LibFeaturesOpts { alloc: alloc });
        seen(features.scan_by::<J, _>(&One(text), 0, ROOT, &cfg).map(|()| features.listed(alloc)))
    }

    /// A drawn crate of those items, with doc comments of drawn length between them, lists the
    /// same features (or is refused at the same byte for the same reason) by both searches.
    #[test]
    fn vector_scans_as_scalar() {
        check(Config::cases(512), |tape: &Tape| {
            let alloc = heap().alloc();
            let mut text = AString::new_in(alloc);
            let items = tape.int_in(0..=12usize, "items");
            for _ in 0..items {
                let doc = tape.int_in(0..=200usize, "doc bytes");
                if doc > 0 {
                    text.push_str("/// ");
                    for _ in 0..doc {
                        text.push('x');
                    }
                    text.push('\n');
                }
                text.push_str(tape.one_of(ITEMS, "item"));
            }
            let want = scan_one::<Scalar>(text.as_str(), alloc);
            let got = scan_one::<Vector>(text.as_str(), alloc);
            assert_eq!(got, want, "the crate:\n{}", text.as_str());
        });
    }

    /// Files read from disk, each kept for the test's life.
    struct Disk;

    impl SourceTexts<'static> for Disk {
        fn read(&self, _krate: usize, path: &str) -> Option<&'static str> {
            let text = std::fs::read_to_string(path).ok()?;
            Some(StdString::leak(text))
        }
    }

    /// The tree's own `library/` (core, alloc, std and the crates of `staged_api` std reaches)
    /// under the host's cfg lists the same by both searches. Prints the files.
    #[test]
    fn vector_scans_the_library_as_scalar() {
        let alloc = heap().alloc();
        let library = StdString::leak(std::format!("{}/../../library", env!("CARGO_MANIFEST_DIR")));
        let atoms: &[(&str, Option<&str>)] = &[("target_arch", Some("aarch64")), ("target_os", Some("macos")), ("target_family", Some("unix")), ("unix", None), ("target_pointer_width", Some("64")), ("target_endian", Some("little")), ("panic", Some("unwind"))];
        let cfg = Cfg::from_atoms(atoms, alloc);
        let mut by_scalar = LibFeatures::new(LibFeaturesOpts { alloc: alloc });
        let mut by_vector = LibFeatures::new(LibFeaturesOpts { alloc: alloc });
        for krate in ["core", "alloc", "std", "std_detect", "unwind", "panic_unwind", "panic_abort"] {
            let root = StdString::leak(std::format!("{library}/{krate}/src/lib.rs"));
            let scalar = by_scalar.scan_by::<Scalar, _>(&Disk, 0, root, &cfg);
            let vector = by_vector.scan_by::<Vector, _>(&Disk, 0, root, &cfg);
            assert_eq!(seen(vector.map(|()| by_vector.listed(alloc))), seen(scalar.map(|()| by_scalar.listed(alloc))), "{krate}");
        }
        println!("lib_features: library/ lists the same by both searches, {} files", by_vector.listed(alloc).files);
    }
}
