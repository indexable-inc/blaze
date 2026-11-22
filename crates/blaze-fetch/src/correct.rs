//! Source corrections (decision D58): a visible table of rows, each replacing one `cfg`
//! predicate in a crate version's fetched, verified source, so a crate that switches its own
//! procedural-macro support off on the sandbox's target (`wasm32-wasip1`) builds unchanged in
//! every project. The macro world stays one truthful target; the crate is corrected, by syntax.
//!
//! The table is data in the repository ([`TABLE`], `crates/blaze-fetch/corrections.json`), read
//! into a frozen [`Table`] that refuses unknown fields. A row names the crate, a version
//! requirement, the files it corrects, the predicate to find and what replaces it, the reason, the
//! decision and the upstream link.
//!
//! Why here: blaze-fetch is where a fetched source is verified and becomes the record every
//! reader reads (`PackView`); the interim module builder and blaze-world both depend on it, so
//! both read the one corrected text. The derived source is keyed by the source's digest and
//! [`Row::digest`] (the readers name both in their keys).
//!
//! Everything here is pure: text and a row in, corrected text and the replaced ranges out. A
//! file is lexed by the tree's lexer (`blaze_lex::lexer::tokenize`); a match is a run of
//! significant tokens (whitespace and comments skipped) equal to the row's predicate, starting
//! right after the `(` or `,` of a `cfg(..)` or `cfg_attr(..)` attribute's argument tree, so it
//! is a whole token subtree. Never a text match. No recursion: brackets are counted in loops.

use core::alloc::Allocator;
use core::fmt;
use core::ops::Range;

use blaze_lex::Cpu;
use blaze_lex::lexer::{FrontmatterAllowed, TokenKind, tokenize};
use blaze_own::{String, Vec};
use blaze_semver::{Version, VersionReq};

/// The table as checked in.
pub const TABLE: &str = include_str!("../corrections.json");

/// One correction.
pub struct Row<A: Allocator> {
    /// The crate's name, as its lock entry spells it.
    pub name: String<A>,
    /// The versions it applies to; each one it applies to must match in every file listed.
    pub versions: VersionReq<A>,
    /// The files corrected, relative to the crate's root.
    pub files: Vec<String<A>, A>,
    /// The predicate to find, as Rust tokens.
    pub find: String<A>,
    /// What replaces it, as Rust tokens.
    pub replace: String<A>,
    /// Why, in a sentence.
    pub reason: String<A>,
    /// The decision that admitted the row (`D58`).
    pub decision: String<A>,
    /// The crate's repository.
    pub upstream: String<A>,
    /// The upstream issue, empty while none is posted.
    pub issue: String<A>,
    /// The bytes the row was checked against, by content. When the list is non-empty, each file
    /// the row corrects must be one of these bytes for that version, or the correction is
    /// [`Refused::Changed`]. An empty list (a version range: synstructure) is held only by the
    /// predicate's match ([`Refused::Stale`]).
    pub digests: Vec<FileDigest<A>, A>,
}

blaze_data::fields!(for<A> Row<A> { name, versions, files, find, replace, reason, decision, upstream, issue, digests });

/// One file's bytes a row was checked against: its SHA-256 in lowercase hex, as fetched.
pub struct FileDigest<A: Allocator> {
    /// The version, as a lock spells it.
    pub version: String<A>,
    /// The file, one the row lists.
    pub file: String<A>,
    pub sha256: String<A>,
}

blaze_data::fields!(for<A> FileDigest<A> { version, file, sha256 });

/// The table.
pub struct Table<A: Allocator> {
    pub rows: Vec<Row<A>, A>,
}

blaze_data::fields!(for<A> Table<A> { rows });

/// Why the table or a correction was refused; each names the row and, where there is one, the
/// file.
#[derive(Debug)]
pub enum Refused<'t> {
    /// A row's `find` or `replace` is not a balanced, non-empty token tree.
    Pattern { name: &'t str, decision: &'t str, which: &'static str },
    /// A row lists no files.
    NoFiles { name: &'t str, decision: &'t str },
    /// A file a row lists is not in the crate.
    MissingFile { name: &'t str, version: &'t str, file: &'t str, decision: &'t str },
    /// A file a row lists is not UTF-8.
    NotText { name: &'t str, version: &'t str, file: &'t str, decision: &'t str },
    /// A row matched nothing in a file of a version it names: a stale row.
    Stale { name: &'t str, version: &'t str, file: &'t str, decision: &'t str },
    /// A row's digest names a file the row does not list, or is not 64 lowercase hex digits.
    Digest { name: &'t str, decision: &'t str, file: &'t str },
    /// A file's bytes are not ones the row's digests name: the row was checked against other text.
    Changed { name: &'t str, version: &'t str, file: &'t str, decision: &'t str, found: [u8; 32] },
}

impl fmt::Display for Refused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Pattern { name, decision, which } => {
                write!(f, "source correction {name} ({decision}): its `{which}` is not a balanced, non-empty token tree")
            }
            Refused::NoFiles { name, decision } => write!(f, "source correction {name} ({decision}) lists no files"),
            Refused::MissingFile { name, version, file, decision } => {
                write!(f, "source correction {name} ({decision}): {name} {version} has no file {file}")
            }
            Refused::NotText { name, version, file, decision } => {
                write!(f, "source correction {name} ({decision}): {name} {version} {file} is not UTF-8")
            }
            Refused::Stale { name, version, file, decision } => write!(
                f,
                "source correction {name} ({decision}) is stale: its predicate matches nothing in {name} {version} {file}; fix or remove the row"
            ),
            Refused::Digest { name, decision, file } => write!(
                f,
                "source correction {name} ({decision}): its digest for {file} is not 64 lowercase hex digits of a file the row lists"
            ),
            Refused::Changed { name, version, file, decision, found } => {
                write!(f, "source correction {name} ({decision}) was checked against other bytes: {name} {version} {file} has SHA-256 ")?;
                for byte in found {
                    write!(f, "{byte:02x}")?;
                }
                write!(f, ", which none of the row's `digests` names; check the predicate still means the same there, then fix the digest")
            }
        }
    }
}

impl core::error::Error for Refused<'_> {}

/// A file's text after a row: the same text by reference when nothing matched.
pub enum Text<'t, A: Allocator> {
    Same(&'t str),
    New(String<A>),
}

impl<A: Allocator> Text<'_, A> {
    pub fn as_str(&self) -> &str {
        match self {
            Text::Same(text) => text,
            Text::New(text) => text,
        }
    }
}

/// One file corrected.
pub struct Corrected<'t, A: Allocator> {
    pub text: Text<'t, A>,
    /// The byte ranges of the original text that were replaced, in order.
    pub replaced: Vec<Range<u32>, A>,
}

/// A significant token: its kind and its byte range.
#[derive(Clone, Copy)]
struct Sig {
    kind: TokenKind,
    start: u32,
    end: u32,
}

fn significant<A: Allocator>(text: &str, cpu: Cpu, alloc: A) -> Vec<Sig, A> {
    let mut out = Vec::new_in(alloc);
    let mut at = 0u32;
    for token in tokenize(text, FrontmatterAllowed::No, cpu) {
        let start = at;
        at += token.len;
        if !matches!(token.kind, TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }) {
            out.push(Sig { kind: token.kind, start, end: at });
        }
    }
    out
}

fn depth_step(kind: TokenKind) -> i32 {
    match kind {
        TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => 1,
        TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => -1,
        _ => 0,
    }
}

/// Non-empty, and balanced with no prefix closing below zero.
fn balanced(tokens: &[Sig]) -> bool {
    let mut depth = 0i32;
    for t in tokens {
        depth += depth_step(t.kind);
        if depth < 0 {
            return false;
        }
    }
    depth == 0 && !tokens.is_empty()
}

fn same(text: &str, a: Sig, pattern: &str, b: Sig) -> bool {
    a.kind == b.kind && text[a.start as usize..a.end as usize] == pattern[b.start as usize..b.end as usize]
}

/// The index of the token closing the bracket opened at `open`, or `None` when unclosed.
fn closing(tokens: &[Sig], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, t) in tokens.iter().enumerate().skip(open) {
        depth += depth_step(t.kind);
        if depth == 0 {
            return Some(i);
        }
    }
    None
}

/// Whether `text` is `digest` in lowercase hex.
fn hex_is(text: &str, digest: &[u8; 32]) -> bool {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let text = text.as_bytes();
    text.len() == 64
        && digest.iter().enumerate().all(|(i, b)| text[2 * i] == HEX[usize::from(b >> 4)] && text[2 * i + 1] == HEX[usize::from(b & 15)])
}

impl<A: Allocator + Clone> Row<A> {
    /// Whether the row names this crate version.
    pub fn applies<B: Allocator>(&self, name: &str, version: &Version<B>) -> bool {
        self.name.as_str() == name && self.versions.matches(version)
    }

    /// The row's digest: SHA-256 over every field, each tagged and ended by a byte UTF-8 never
    /// holds, so no two rows share one. A derived source is keyed by its source's digest and
    /// this.
    pub fn digest(&self) -> [u8; 32] {
        struct Feed<'h>(&'h mut blaze_sha256::Sha256);
        impl fmt::Write for Feed<'_> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                self.0.update(s.as_bytes());
                Ok(())
            }
        }
        let mut hasher = blaze_sha256::Sha256::new();
        let field = |tag: &str, text: &str, h: &mut blaze_sha256::Sha256| {
            h.update(tag.as_bytes()).update(&[0xfe]).update(text.as_bytes()).update(&[0xff]);
        };
        field("source-correction-v1", "", &mut hasher);
        field("name", &self.name, &mut hasher);
        hasher.update(b"versions").update(&[0xfe]);
        // Writing into a hasher cannot fail.
        let _ = fmt::write(&mut Feed(&mut hasher), format_args!("{}", self.versions));
        hasher.update(&[0xff]);
        for file in self.files.iter() {
            field("file", file, &mut hasher);
        }
        field("find", &self.find, &mut hasher);
        field("replace", &self.replace, &mut hasher);
        field("reason", &self.reason, &mut hasher);
        field("decision", &self.decision, &mut hasher);
        field("upstream", &self.upstream, &mut hasher);
        field("issue", &self.issue, &mut hasher);
        // An empty list hashes nothing: a row without digests keeps its key.
        for pin in self.digests.iter() {
            field("digest-version", &pin.version, &mut hasher);
            field("digest-file", &pin.file, &mut hasher);
            field("digest-sha256", &pin.sha256, &mut hasher);
        }
        hasher.finish()
    }

    /// Checks the row's shape: files listed, `find` and `replace` balanced token trees.
    pub fn check(&self, cpu: Cpu) -> Result<(), Refused<'_>> {
        let (name, decision) = (self.name.as_str(), self.decision.as_str());
        if self.files.is_empty() {
            return Err(Refused::NoFiles { name, decision });
        }
        for pin in self.digests.iter() {
            let (file, hex) = (pin.file.as_str(), pin.sha256.as_bytes());
            let listed = self.files.iter().any(|it| it.as_str() == file);
            if !listed || hex.len() != 64 || !hex.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return Err(Refused::Digest { name, decision, file });
            }
        }
        let alloc = self.files.allocator().clone();
        if !balanced(&significant(&self.find, cpu, alloc.clone())) {
            return Err(Refused::Pattern { name, decision, which: "find" });
        }
        if !balanced(&significant(&self.replace, cpu, alloc)) {
            return Err(Refused::Pattern { name, decision, which: "replace" });
        }
        Ok(())
    }

    /// `text` with every occurrence of the row's predicate inside a `cfg(..)` or `cfg_attr(..)`
    /// attribute replaced. Nothing matched: the text by reference and no ranges.
    pub fn correct_text<'t, B: Allocator + Clone>(&self, text: &'t str, cpu: Cpu, alloc: B) -> Corrected<'t, B> {
        let pattern = significant(&self.find, cpu, alloc.clone());
        let tokens = significant(text, cpu, alloc.clone());
        let mut replaced: Vec<Range<u32>, B> = Vec::new_in(alloc.clone());
        let n = pattern.len();
        let mut i = 0;
        while i < tokens.len() {
            // `#` `!`? `[` `cfg` | `cfg_attr` `(`
            if tokens[i].kind != TokenKind::Pound {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            if tokens.get(j).is_some_and(|t| t.kind == TokenKind::Bang) {
                j += 1;
            }
            let is_cfg = tokens.get(j).is_some_and(|t| t.kind == TokenKind::OpenBracket)
                && tokens.get(j + 1).is_some_and(|t| {
                    t.kind == TokenKind::Ident && matches!(&text[t.start as usize..t.end as usize], "cfg" | "cfg_attr")
                })
                && tokens.get(j + 2).is_some_and(|t| t.kind == TokenKind::OpenParen);
            if !is_cfg {
                i += 1;
                continue;
            }
            let open = j + 2;
            let Some(close) = closing(&tokens, open) else { break };
            let mut k = open + 1;
            while n > 0 && k + n <= close {
                let after_separator = matches!(tokens[k - 1].kind, TokenKind::OpenParen | TokenKind::Comma);
                if after_separator && (0..n).all(|p| same(text, tokens[k + p], &self.find, pattern[p])) {
                    replaced.push(tokens[k].start..tokens[k + n - 1].end);
                    k += n;
                } else {
                    k += 1;
                }
            }
            i = close + 1;
        }
        if replaced.is_empty() {
            return Corrected { text: Text::Same(text), replaced };
        }
        let mut out = String::new_in(alloc);
        let mut at = 0usize;
        for range in replaced.iter() {
            out.push_str(&text[at..range.start as usize]);
            out.push_str(&self.replace);
            at = range.end as usize;
        }
        out.push_str(&text[at..]);
        Corrected { text: Text::New(out), replaced }
    }

    /// Every file the row lists, corrected, in the row's order; `files` gives a file's bytes
    /// (the reader's, from the verified source). A listed file that is missing, not UTF-8 or
    /// with no match is refused: a stale row never passes silently.
    pub fn correct_crate<'t, B: Allocator + Clone>(
        &'t self,
        version: &'t str,
        mut files: impl FnMut(&str) -> Option<&'t [u8]>,
        cpu: Cpu,
        alloc: B,
    ) -> Result<Vec<(&'t str, Corrected<'t, B>), B>, Refused<'t>> {
        self.check(cpu)?;
        let (name, decision) = (self.name.as_str(), self.decision.as_str());
        let mut out = Vec::new_in(alloc.clone());
        for file in self.files.iter() {
            let file = file.as_str();
            let bytes = files(file).ok_or(Refused::MissingFile { name, version, file, decision })?;
            if !self.digests.is_empty() {
                let mut hasher = blaze_sha256::Sha256::new();
                hasher.update(bytes);
                let found = hasher.finish();
                let pinned = self.digests.iter().any(|pin| pin.version.as_str() == version && pin.file.as_str() == file && hex_is(&pin.sha256, &found));
                if !pinned {
                    return Err(Refused::Changed { name, version, file, decision, found });
                }
            }
            let text = core::str::from_utf8(bytes).map_err(|_| Refused::NotText { name, version, file, decision })?;
            let corrected = self.correct_text(text, cpu, alloc.clone());
            if corrected.replaced.is_empty() {
                return Err(Refused::Stale { name, version, file, decision });
            }
            out.push((file, corrected));
        }
        Ok(out)
    }
}

impl<A: Allocator + Clone> Table<A> {
    /// The table from its JSON text; unknown fields are refused, every row's shape checked.
    pub fn read(text: &str, cpu: Cpu, alloc: A) -> Result<Table<A>, TableError<A>> {
        let table: Table<A> =
            blaze_json::from_slice_in(text.as_bytes(), blaze_json::Limits::DEFAULT, alloc).map_err(TableError::Json)?;
        for (at, row) in table.rows.iter().enumerate() {
            if let Err(why) = row.check(cpu) {
                return Err(TableError::Row { at, why: alloc_text(&why, table.rows.allocator().clone()) });
            }
        }
        Ok(table)
    }

    /// The row naming this crate version, if any (the table holds at most one per crate
    /// version: [`Table::read`]'s caller keeps that by review of a small file).
    pub fn find<B: Allocator>(&self, name: &str, version: &Version<B>) -> Option<&Row<A>> {
        self.rows.iter().find(|row| row.applies(name, version))
    }

    /// [`Table::find`] by a lock's version text; a version that does not parse names no row
    /// (a lock's versions always parse).
    pub fn find_text(&self, name: &str, version: &str) -> Option<&Row<A>> {
        if !self.rows.iter().any(|row| row.name.as_str() == name) {
            return None;
        }
        let version: Version<A> =
            Version::parse_in(version, blaze_semver::Limits::CARGO, self.rows.allocator().clone()).ok()?;
        self.find(name, &version)
    }

    /// A package's pin in a proc-macro module's dependency closure (its identity's `deps`
    /// line): the lock's `pin`, then ` corrected <row digest>` when a row applies, so the
    /// module key changes when the row does. The one home of that line: the interim builder and
    /// blaze-proc-macro's `LockedPackages::closure` both call it.
    pub fn pin(&self, name: &str, version: &str, pin: &str) -> alloc::string::String {
        let mut out = alloc::string::String::from(pin);
        if let Some(row) = self.find_text(name, version) {
            out.push_str(" corrected ");
            for byte in row.digest() {
                out.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
                out.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
            }
        }
        out
    }

    /// The checked-in table, [`TABLE`]. Its reading is a test (`tests::synstructure_seven_sites`),
    /// so a refusal here is a tree whose own data is broken: it panics naming the row.
    pub fn checked_in(alloc: A) -> Table<A> {
        match Table::read(TABLE, Cpu::BASELINE, alloc) {
            Ok(table) => table,
            Err(error) => panic!("{error}"),
        }
    }
}

fn alloc_text<A: Allocator + Clone>(what: &dyn fmt::Display, alloc: A) -> String<A> {
    struct Into<'s, A: Allocator>(&'s mut String<A>);
    impl<A: Allocator> fmt::Write for Into<'_, A> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.0.push_str(s);
            Ok(())
        }
    }
    let mut out = String::new_in(alloc);
    // Writing into a string cannot fail.
    let _ = fmt::write(&mut Into(&mut out), format_args!("{what}"));
    out
}

/// Why the table could not be read.
pub enum TableError<A: Allocator> {
    Json(blaze_json::FromJsonError<A>),
    Row { at: usize, why: String<A> },
}

impl<A: Allocator> fmt::Display for TableError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::Json(error) => write!(f, "crates/blaze-fetch/corrections.json: {error}"),
            TableError::Row { at, why } => write!(f, "crates/blaze-fetch/corrections.json row {at}: {}", why.as_str()),
        }
    }
}

impl<A: Allocator> fmt::Debug for TableError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests;
