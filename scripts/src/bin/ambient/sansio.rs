//! The sans-IO gate (decision D170, convention C7, docs/v2/plan/design/end-state.md): per crate,
//! what stands between it and "a sans-IO state machine on bstd with nothing ambient", counted
//! from the tree's TOKENS (blaze-lex's `tokenize`, the lexer blaze compiles with), never from
//! lines of text: comments, strings, characters and raw strings are tokens the count never reads,
//! and test code is cut by the token structure of its attribute and item. Each crate is held to
//! its row of tools/ratchets/sans-io.txt, which only falls, and each crate named in
//! tools/ratchets/sans-io-done.txt (the crates that claim C7) is held at zero in every column,
//! its third-party row (tools/ratchets/third-party.txt, thirdparty.rs's, the one home of that
//! count) at zero, and its root denying the gate's lints.
//!
//!     blaze run -q -p scripts --bin ambient -- '{"command": "sansio-check", "base": "HEAD"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "sansio-write"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "sansio-report"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "sansio-sites"}'
//!
//! Which files are test code is the walk's (ambient.rs `walk`: test directories and files,
//! `#![cfg(test)]`, modules declared in test code), the ONE definition the other counts read;
//! inside a file that is code, an item under `#[cfg(test)]`, `#[cfg(all(test, ..))]` or a
//! `#[..::test]` attribute is cut here by its tokens.

use super::reach::{CALLS, Column as ReachColumn};
use super::*;
use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind};

/// What ambient.rs's `DOC` says of the sans-IO gate, appended to it in the usage error.
pub(super) const SANS_DOC: &str = r#"
The sans-IO gate (D170, C7): what keeps each crate from being a sans-IO state machine on bstd
with nothing ambient, read from the tree's tokens (blaze-lex), comments and strings never
read. Its list tools/ratchets/sans-io.txt, rows `<crate><TAB>` and eighteen columns, each a count
of sites outside test code unless it says tests:
  std                 paths whose first segment is `std`, and `extern crate std`
  alloc               the same for `alloc`
  global              each `Global` named (its definition not)
  global_tests        the same in test code
  alloc_global        `Alloc::global()` and `SharedAlloc::global()` called
  alloc_global_tests  the same in test code (C1: none anywhere)
  implicit            an allocating call with no allocator named: `Vec::new(`,
                      `Vec::with_capacity(`, `Box::new(`, `String::new(`, `String::from(`,
                      `Rc::new(`, `Arc::new(`, `vec![..]`, `format!(..)`, `.to_string()`,
                      `.to_owned()`, `.to_vec()` (blaze-lint's `implicit_allocator` table)
  statics             `thread_local!`, `static mut`, and a `static` whose type names a cell, a
                      lock, a once or an atomic (`Cell`, `RefCell`, `UnsafeCell`, `OnceCell`,
                      `OnceLock`, `LazyLock`, `LazyCell`, `Mutex`, `RwLock`, `Condvar`, `Lock`,
                      `SyncUnsafeCell`, `Atomic*`)
  clock, env, random  the reach ratchet's calls (reach.rs's table), read as tokens
  io                  direct IO: paths `std::{fs, net, process, io, os}`, `libc::`, `rustix::`
                      (but the calls reach.rs files under env), `fs::<f>(`, `File::open(`,
                      `File::create(`, `OpenOptions::new(`, `Command::new(`, sockets'
                      `connect`/`bind`, `print!`, `println!`, `eprint!`, `eprintln!`, `dbg!`,
                      the reach ratchet's exit, world and test-root calls, and `Os::process()`
                      outside a `fn main`
  no_std              1 when the crate's root says neither `#![no_std]` nor
                      `#![cfg_attr(not(test), no_std)]`
  unmarked            1 when the root says neither `#![blaze::sans_io]` (a core) nor
                      `#![blaze::driver]` (a driver: the program's entry, which holds handles and
                      makes straight-line calls)
  breaks              in code marked sans-IO (a root's `#![blaze::sans_io]`, a module's inner
                      `#![blaze::sans_io]` and every module it declares, an item's
                      `#[blaze::sans_io]`): each clock, env, random or io site, and each blocking
                      call (`.recv()`, `.join()`, `.wait()`, `.lock()`, `thread::spawn(`,
                      `thread::park(`, `thread::scope(`, `block_on(`)
  bare                D173: 1 when the shipped binaries reach the crate (tools/ratchets/reached.txt,
                      thirdparty.rs's) and it is not `no_std` without `alloc`: its root says
                      `#![no_std]`, its shipped code names no `alloc::` path, `extern crate alloc`
                      or `extern crate std`, its manifest names no third-party package by a normal
                      or build dependency, and every workspace crate it names is bare too. A crate
                      of the exemption table (`EXEMPT`, each row with why) is never 1 and does not
                      fail the crates that name it
  locks_constructed   D179: each lock constructed, `Lock::new`, `Lock::unrecorded`, `Lock::probed`,
                      `ReadLock::new`, `ReadLock::unrecorded`, `RwLock::new`, `RankedLock::new`,
                      `SharedMap::new_in` (a turbofish between allowed, a lowercase module path
                      before it allowed), outside a `use`; only falls
  locks_without_reason
                      the same with no `#[blaze::lock_exception(..)]` over it: on its statement,
                      field, `static`, `const` or item, to the first `;` or `,` outside brackets,
                      the `}` of its first block unless `else` follows, or the bracket that closes
                      around it (blaze-lint's `lock-without-reason`, kept equal by hand); falls to
                      0, then is held there
A crate is the nearest directory with a Cargo.toml under crates/ (ambient.rs `crate_of`).

    command   sansio-check   each crate at or under its row; one over is refused, naming the
                             column, the count and its first three sites (the new ones given
                             `base`); a row raised since `base` is refused; each crate of
                             tools/ratchets/sans-io-done.txt must be 0 in every column but
                             `locks_constructed` (a lock with its reason is D179's exception), have no
                             third-party row, and its root must deny `blaze::std_use`,
                             `blaze::implicit_allocator`, `blaze::process_static` and, for a
                             core, `blaze::sans_io_core`; and a crate whose root carries a
                             `#![blaze::..]` inner attribute while its library names one of its
                             own `#[macro_export]` macros as `crate::name!` or `$crate::name!`
                             is refused by name (rustc refuses that path under a root tool
                             attribute; agents/annoyances/crate-root-tool-attribute-hides-macros.json)
              sansio-write   writes the list from the tree: the first write takes the tree as
                             it is; after it a column is the lower of the row and the count (a
                             row never rises by a write; raise one by hand, with a comment above
                             it saying why, which every write keeps)
              sansio-report  one table, a row a crate (every crate with a root), a column a
                             count, the third-party columns from tools/ratchets/third-party.txt
              sansio-sites   each site outside test code: crate, path:line, column
"#;

/// The list, from the root.
pub(super) const SANS_LIST: &str = "tools/ratchets/sans-io.txt";
/// The crates that claim C7, one a line.
pub(super) const DONE_LIST: &str = "tools/ratchets/sans-io-done.txt";
/// thirdparty.rs's list, read for its rows (it is the one home of that count).
const THIRD_PARTY_LIST: &str = "tools/ratchets/third-party.txt";

const SANS_HEADER: &str = "# What keeps each crate from being a sans-IO state machine on bstd with nothing ambient\n\
                           # (D170, C7), by crate (its directory under crates/), read from the tree's tokens: paths of\n\
                           # `std` and `alloc`, `Global` named (in shipped code and in tests), `Alloc::global()` (the\n\
                           # same), allocating calls with no allocator named, statics with state, clock, environment\n\
                           # and randomness reads, direct IO, a root that is not `#![no_std]`, a root that says neither\n\
                           # `#![blaze::sans_io]` nor `#![blaze::driver]`, blocking or IO calls inside code marked\n\
                           # sans-IO, a crate the binaries reach that is not `no_std` without `alloc` (D173), the locks\n\
                           # constructed, and those with no `#[blaze::lock_exception(..)]` (D179). Only falls; a crate with no row is held to 0 in every column. Written by\n\
                           # scripts/src/bin/ambient.rs (`{\"command\": \"sansio-write\"}`), checked by\n\
                           # `{\"command\": \"sansio-check\"}`; tools/ratchets/sans-io-done.txt holds its crates at 0 (a lock with its reason aside).\n\
                           # crate\tstd\talloc\tglobal\tglobal_tests\talloc_global\talloc_global_tests\timplicit\tstatics\tclock\tenv\trandom\tio\tno_std\tunmarked\tbreaks\tbare\tlocks_constructed\tlocks_without_reason\n";

const WIDTH: usize = 18;

/// The list's columns, in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Col {
    Std,
    Alloc,
    Global,
    GlobalTests,
    AllocGlobal,
    AllocGlobalTests,
    Implicit,
    Statics,
    Clock,
    Env,
    Random,
    Io,
    NoStd,
    Unmarked,
    Breaks,
    /// D173: 1 when the shipped binaries reach the crate and it is not `no_std` without
    /// `alloc` ([`bare_crates`]).
    Bare,
    /// D179: each lock constructed outside test code (blaze-lint's `lock-without-reason`
    /// constructors), reason or not: only falls.
    Locks,
    /// D179: the same with no `#[blaze::lock_exception(..)]` over it: falls to 0, then held.
    LocksNoReason,
}

const COLS: [Col; WIDTH] = [
    Col::Std,
    Col::Alloc,
    Col::Global,
    Col::GlobalTests,
    Col::AllocGlobal,
    Col::AllocGlobalTests,
    Col::Implicit,
    Col::Statics,
    Col::Clock,
    Col::Env,
    Col::Random,
    Col::Io,
    Col::NoStd,
    Col::Unmarked,
    Col::Breaks,
    Col::Bare,
    Col::Locks,
    Col::LocksNoReason,
];

/// A crate's counts, by [`Col::at`].
type SansRow = [usize; WIDTH];

impl Col {
    const fn at(self) -> usize {
        self as usize
    }

    const fn name(self) -> &'static str {
        match self {
            Col::Std => "std",
            Col::Alloc => "alloc",
            Col::Global => "global",
            Col::GlobalTests => "global_tests",
            Col::AllocGlobal => "alloc_global",
            Col::AllocGlobalTests => "alloc_global_tests",
            Col::Implicit => "implicit",
            Col::Statics => "statics",
            Col::Clock => "clock",
            Col::Env => "env",
            Col::Random => "random",
            Col::Io => "io",
            Col::NoStd => "no_std",
            Col::Unmarked => "unmarked",
            Col::Breaks => "breaks",
            Col::Bare => "bare",
            Col::Locks => "locks_constructed",
            Col::LocksNoReason => "locks_without_reason",
        }
    }

    const fn what(self) -> &'static str {
        match self {
            Col::Std => "paths of `std` outside tests",
            Col::Alloc => "paths of `alloc` outside tests",
            Col::Global => "`Global`s named outside tests",
            Col::GlobalTests => "`Global`s named in tests",
            Col::AllocGlobal => "`Alloc::global()` calls outside tests",
            Col::AllocGlobalTests => "`Alloc::global()` calls in tests",
            Col::Implicit => "allocating calls with no allocator named",
            Col::Statics => "statics with state",
            Col::Clock => "reads of the clock",
            Col::Env => "reads of the environment",
            Col::Random => "draws of randomness",
            Col::Io => "direct IO calls or paths",
            Col::NoStd => "root without `#![no_std]`",
            Col::Unmarked => "root that says neither `#![blaze::sans_io]` nor `#![blaze::driver]`",
            Col::Breaks => "blocking or IO calls inside code marked sans-IO",
            Col::Bare => "crate the binaries reach that is not `#![no_std]` without `alloc` (D173)",
            Col::Locks => "locks constructed outside tests (D179)",
            Col::LocksNoReason => "locks constructed with no `#[blaze::lock_exception(..)]` (D179)",
        }
    }

    const fn instead(self) -> &'static str {
        match self {
            Col::Std | Col::Alloc => {
                "take it from `core` or bstd (blaze-own's collections, blaze-hash's maps, the capabilities in `bstd`)"
            }
            Col::Global | Col::GlobalTests | Col::AllocGlobal | Col::AllocGlobalTests => {
                "take `alloc: Alloc<'_>` from the caller (C1); a test takes `blaze_own::testing` 's heap from its root"
            }
            Col::Implicit => "the `_in` form with the caller's allocator (`Vec::new_in(alloc)`, `format_in!(alloc, ..)`)",
            Col::Statics => {
                "keep the state in the machine and pass it in (C3); only the foreign heap, the profile ring and the signal handler are process statics, each a row of crates/blaze-lint/src/allowed.rs"
            }
            Col::Clock | Col::Env | Col::Random => "take the capability from the caller (`Clock`, `Env`, `Rng`), as the reach ratchet says",
            Col::Io => "the core returns an effect to its driver; a driver uses bstd's handles (`Out`, `Dir<_>`), passed from `main`",
            Col::NoStd => "put `#![no_std]` at the root, with `extern crate std;` under `#[cfg(test)]` if its tests need it",
            Col::Unmarked => "say `#![blaze::sans_io]` at the root of a core, or `#![blaze::driver]` at the root of the program's entry",
            Col::Breaks => "return the wait or the IO as an effect from `step` and let the driver do it (C5)",
            Col::Bare => {
                "`#![no_std]` at the root, no `extern crate alloc` or `alloc::` path and no `extern crate std` outside `cfg(test)`, no third-party \
                 dependency, and every workspace dependency bare too (`sansio-report` names which fails)"
            }
            Col::Locks => {
                "publish the state (one writer, readers take a snapshot), gather it (each worker its own part, joined at the end) or claim it (one owner at a time, handed on): D179's three shapes"
            }
            Col::LocksNoReason => {
                "use one of D179's shapes, or say why none fits: `#[blaze::lock_exception(tried = \"published\"|\"gathered\"|\"claimed\", why = \"..\")]` on the statement, field, `static` or item"
            }
        }
    }
}

/// What one token site is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum What {
    Std,
    Alloc,
    Global,
    AllocGlobal,
    Implicit,
    Static,
    Clock,
    Env,
    Random,
    Io,
    /// A call that blocks the thread: counted only inside code marked sans-IO, as `breaks`.
    Blocking,
    /// `extern crate std` (its `std` is a [`What::Std`] too): outside test code it fails the
    /// crate's `bare` (D173), and is no column of its own.
    ExternStd,
    /// A lock constructed (D179), reason or not.
    Lock,
    /// A lock constructed with no `#[blaze::lock_exception(..)]` over it (its [`What::Lock`] is
    /// pushed too).
    LockNoReason,
}

impl What {
    /// Its column outside test code.
    const fn shipped(self) -> Option<Col> {
        match self {
            What::Std => Some(Col::Std),
            What::Alloc => Some(Col::Alloc),
            What::Global => Some(Col::Global),
            What::AllocGlobal => Some(Col::AllocGlobal),
            What::Implicit => Some(Col::Implicit),
            What::Static => Some(Col::Statics),
            What::Clock => Some(Col::Clock),
            What::Env => Some(Col::Env),
            What::Random => Some(Col::Random),
            What::Io => Some(Col::Io),
            What::Lock => Some(Col::Locks),
            What::LockNoReason => Some(Col::LocksNoReason),
            What::Blocking | What::ExternStd => None,
        }
    }

    /// Its column in test code: only the heap's (C1 holds tests too).
    const fn in_tests(self) -> Option<Col> {
        match self {
            What::Global => Some(Col::GlobalTests),
            What::AllocGlobal => Some(Col::AllocGlobalTests),
            _ => None,
        }
    }

    /// Whether it breaks code marked sans-IO.
    const fn breaks(self) -> bool {
        matches!(self, What::Clock | What::Env | What::Random | What::Io | What::Blocking)
    }
}

/// One significant token: its kind, its text, its line (from 1), and what the attributes around
/// it make it.
#[derive(Debug, Clone, Copy)]
struct Tok<'t> {
    kind: TokenKind,
    text: &'t str,
    line: usize,
    /// Under a test attribute, or in a file that is test code.
    test: bool,
    /// Under `blaze::sans_io`.
    core: bool,
    /// In the body of a `fn main`.
    main: bool,
}

impl<'t> Tok<'t> {
    /// The identifier, `r#` taken off a raw one.
    fn word(&self) -> Option<&'t str> {
        match self.kind {
            TokenKind::Ident => Some(self.text),
            TokenKind::RawIdent => self.text.get(2..),
            _ => None,
        }
    }
}

/// The tokens of `text` that are not whitespace or comments, each with its line.
fn lex(text: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut line = 1;
    // Bounded: the lexer moves forward by at least one byte a token, to the end of the text.
    for token in blaze_lex::tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let end = at + token.len as usize;
        let piece = &text[at..end];
        match token.kind {
            TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. } | TokenKind::Frontmatter { .. } => {}
            kind => out.push(Tok { kind, text: piece, line, test: false, core: false, main: false }),
        }
        line += piece.bytes().filter(|b| *b == b'\n').count();
        at = end;
    }
    out
}

fn kind_at(toks: &[Tok<'_>], at: usize) -> Option<TokenKind> {
    toks.get(at).map(|it| it.kind)
}

fn word_at<'t>(toks: &[Tok<'t>], at: usize) -> Option<&'t str> {
    toks.get(at).and_then(Tok::word)
}

/// `::` at `at` and `at + 1`.
fn sep_at(toks: &[Tok<'_>], at: usize) -> bool {
    kind_at(toks, at) == Some(TokenKind::Colon) && kind_at(toks, at + 1) == Some(TokenKind::Colon)
}

const fn opens(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace)
}

const fn closes(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace)
}

/// The token that closes the bracket opened at `open`.
fn matching(toks: &[Tok<'_>], open: usize) -> Option<usize> {
    let mut depth: usize = 0;
    // Bounded: one turn per token, to the end at most.
    for (n, tok) in toks.iter().enumerate().skip(open) {
        if opens(tok.kind) {
            depth += 1;
        } else if closes(tok.kind) {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(n);
            }
        }
    }
    None
}

/// An attribute read at its `#`.
#[derive(Debug, Clone, Copy)]
struct AttrAt {
    inner: bool,
    /// Its tokens inside the brackets.
    from: usize,
    to: usize,
    /// The token after its `]`.
    after: usize,
}

fn attribute(toks: &[Tok<'_>], at: usize) -> Option<AttrAt> {
    let inner = kind_at(toks, at + 1) == Some(TokenKind::Bang);
    let open = at + 1 + usize::from(inner);
    if kind_at(toks, open) != Some(TokenKind::OpenBracket) {
        return None;
    }
    let close = matching(toks, open)?;
    Some(AttrAt { inner, from: open + 1, to: close, after: close + 1 })
}

/// What an attribute is, for the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attr {
    /// `cfg(test)`, `cfg(all(test, ..))`, or a path ending in `test` (`test`, `tokio::test(..)`).
    Test,
    /// `blaze::sans_io`.
    SansIo,
    /// `blaze::driver`.
    Driver,
    /// `no_std`, `cfg_attr(not(test), no_std)`.
    NoStd,
    /// `deny(..)` or `forbid(..)`.
    Deny,
    /// The body of a `fn main` (no attribute: a mark of [`mark`]).
    Main,
    Other,
}

fn attr_kind(a: &[Tok<'_>]) -> Attr {
    let w = |at: usize| word_at(a, at);
    let k = |at: usize| kind_at(a, at);
    if a.len() == 1 && w(0) == Some("no_std") {
        return Attr::NoStd;
    }
    if a.len() == 4 && w(0) == Some("blaze") && sep_at(a, 1) {
        match w(3) {
            Some("sans_io") => return Attr::SansIo,
            Some("driver") => return Attr::Driver,
            _ => {}
        }
    }
    if matches!(w(0), Some("deny" | "forbid")) && k(1) == Some(TokenKind::OpenParen) {
        return Attr::Deny;
    }
    if w(0) == Some("cfg") && k(1) == Some(TokenKind::OpenParen) {
        if w(2) == Some("test") && k(3) == Some(TokenKind::CloseParen) && a.len() == 4 {
            return Attr::Test;
        }
        if w(2) == Some("all")
            && k(3) == Some(TokenKind::OpenParen)
            && w(4) == Some("test")
            && matches!(k(5), Some(TokenKind::Comma | TokenKind::CloseParen))
        {
            return Attr::Test;
        }
        return Attr::Other;
    }
    if a.len() == 9
        && w(0) == Some("cfg_attr")
        && w(2) == Some("not")
        && w(4) == Some("test")
        && k(6) == Some(TokenKind::Comma)
        && w(7) == Some("no_std")
    {
        return Attr::NoStd;
    }
    // A path ending in `test`, alone or with arguments.
    let mut at = 0;
    // Bounded: each turn moves past one segment.
    let last = loop {
        let Some(word) = w(at) else { return Attr::Other };
        at += 1;
        if sep_at(a, at) {
            at += 2;
        } else {
            break word;
        }
    };
    if last == "test" && (at == a.len() || k(at) == Some(TokenKind::OpenParen)) { Attr::Test } else { Attr::Other }
}

/// The last token of the item that starts at `from` (after its attribute): past the attributes
/// after it, to its `;` or `,`, or the `}` of its first block, or the token before the bracket
/// that closes around it.
fn item_end(toks: &[Tok<'_>], from: usize) -> usize {
    let last = toks.len().saturating_sub(1);
    let mut at = from;
    // Bounded: each turn moves past one attribute.
    while kind_at(toks, at) == Some(TokenKind::Pound) {
        match attribute(toks, at) {
            Some(attr) => at = attr.after,
            None => break,
        }
    }
    let mut depth: usize = 0;
    // Bounded: one turn per token, to the end at most.
    while at < toks.len() {
        let kind = toks[at].kind;
        if kind == TokenKind::OpenBrace && depth == 0 {
            return matching(toks, at).unwrap_or(last);
        }
        if opens(kind) {
            depth += 1;
        } else if closes(kind) {
            if depth == 0 {
                return at.saturating_sub(1);
            }
            depth -= 1;
        } else if matches!(kind, TokenKind::Semi | TokenKind::Comma) && depth == 0 {
            return at;
        }
        at += 1;
    }
    last
}

/// The `{` of the body of the `fn` whose name is just before `from`, past its parameters,
/// return type and bounds; `None` when a `;` comes first.
fn body_of(toks: &[Tok<'_>], from: usize) -> Option<usize> {
    let mut depth: usize = 0;
    // Bounded: one turn per token, to the end at most.
    for (n, tok) in toks.iter().enumerate().skip(from) {
        match tok.kind {
            TokenKind::OpenBrace if depth == 0 => return Some(n),
            TokenKind::Semi if depth == 0 => return None,
            kind if opens(kind) => depth += 1,
            kind if closes(kind) => depth = depth.checked_sub(1)?,
            _ => {}
        }
    }
    None
}

/// What a file's own inner attributes, at its top, say.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Marks {
    pub(super) no_std: bool,
    pub(super) sans_io: bool,
    pub(super) driver: bool,
    pub(super) test: bool,
    /// The `blaze::` lints its `#![deny(..)]` and `#![forbid(..)]` name.
    pub(super) denied: BTreeSet<String>,
    /// The first `#![blaze::<name>..]` at its top, as `blaze::<name>`: at a crate root it makes
    /// rustc read the crate as macro-expanded ([`macro_path_problems`]).
    pub(super) tool: Option<String>,
}

impl Marks {
    fn add(&mut self, kind: Attr, a: &[Tok<'_>]) {
        if self.tool.is_none()
            && word_at(a, 0) == Some("blaze")
            && sep_at(a, 1)
            && let Some(name) = word_at(a, 3)
        {
            self.tool = Some(format!("blaze::{name}"));
        }
        match kind {
            Attr::NoStd => self.no_std = true,
            Attr::SansIo => self.sans_io = true,
            Attr::Driver => self.driver = true,
            Attr::Test => self.test = true,
            Attr::Deny => {
                for at in 0..a.len() {
                    if word_at(a, at) == Some("blaze") && sep_at(a, at + 1) {
                        if let Some(name) = word_at(a, at + 3) {
                            self.denied.insert(name.to_owned());
                        }
                    }
                }
            }
            Attr::Main | Attr::Other => {}
        }
    }
}

/// Marks each token by the attributes around it (test code, sans-IO code, a `fn main`'s body)
/// and returns what the file's top-level inner attributes say.
fn mark(toks: &mut [Tok<'_>]) -> Marks {
    let mut marks = Marks::default();
    let mut ranges: Vec<(usize, usize, Attr)> = Vec::new();
    // The `{` of each block the walk is in.
    let mut blocks: Vec<usize> = Vec::new();
    let mut at = 0;
    // Bounded: every turn moves `at` forward.
    while at < toks.len() {
        match toks[at].kind {
            TokenKind::OpenBrace => {
                blocks.push(at);
                at += 1;
            }
            TokenKind::CloseBrace => {
                blocks.pop();
                at += 1;
            }
            TokenKind::Pound => {
                let Some(attr) = attribute(toks, at) else {
                    at += 1;
                    continue;
                };
                let kind = attr_kind(&toks[attr.from..attr.to]);
                if attr.inner {
                    match blocks.last() {
                        None => marks.add(kind, &toks[attr.from..attr.to]),
                        Some(&open) if matches!(kind, Attr::Test | Attr::SansIo) => {
                            ranges.push((open, matching(toks, open).unwrap_or(toks.len() - 1), kind));
                        }
                        Some(_) => {}
                    }
                } else if matches!(kind, Attr::Test | Attr::SansIo) {
                    ranges.push((at, item_end(toks, attr.after), kind));
                }
                at = attr.after;
            }
            TokenKind::Ident if toks[at].text == "fn" && word_at(toks, at + 1) == Some("main") => {
                if let Some(open) = body_of(toks, at + 2) {
                    ranges.push((open, matching(toks, open).unwrap_or(toks.len() - 1), Attr::Main));
                }
                at += 1;
            }
            _ => at += 1,
        }
    }
    for (from, to, kind) in ranges {
        for tok in toks.iter_mut().take(to + 1).skip(from) {
            match kind {
                Attr::Test => tok.test = true,
                Attr::SansIo => tok.core = true,
                Attr::Main => tok.main = true,
                _ => {}
            }
        }
    }
    for tok in toks.iter_mut() {
        tok.test |= marks.test;
        tok.core |= marks.sans_io;
    }
    marks
}

/// The definitions: the name after one is defined, not used.
const DEFINING: [&str; 7] = ["struct", "enum", "union", "trait", "type", "fn", "mod"];
/// `<type>::<fn>(` calls that allocate with no allocator named: blaze-lint's `implicit_allocator`
/// sequences (crates/blaze-lint/src/sequences.rs, the `HeapConstructor` rows), kept equal by hand
/// (scripts do not depend on blaze-lint).
const IMPLICIT_CALLS: [(&str, &str); 7] =
    [("Vec", "new"), ("Vec", "with_capacity"), ("Box", "new"), ("String", "new"), ("String", "from"), ("Rc", "new"), ("Arc", "new")];
/// Macros that allocate with no allocator named (the `HeapMacro` rows).
const IMPLICIT_MACROS: [&str; 2] = ["vec", "format"];
/// Methods that allocate with no allocator named (the `HeapMethod` rows).
const IMPLICIT_METHODS: [&str; 3] = ["to_string", "to_owned", "to_vec"];
/// A word in a static's type that makes it state: the names blaze-lint's `process-static` reads
/// (crates/blaze-lint/src/engine.rs `stateful`), kept the same so the count and the lint agree.
fn stateful(word: &str) -> bool {
    word.starts_with("Atomic")
        || word.ends_with("Cell")
        || word.ends_with("Lock")
        || matches!(word, "Mutex" | "Condvar" | "Lazy" | "Once" | "Barrier")
}
/// `std::<module>` paths that are IO.
const IO_MODULES: [&str; 5] = ["fs", "net", "process", "io", "os"];
/// Crates whose every path is a system call.
const IO_CRATES: [&str; 2] = ["libc", "rustix"];
/// `<type>::<fn>(` calls that do IO.
const IO_CALLS: [(&str, &str); 9] = [
    ("File", "open"),
    ("File", "create"),
    ("OpenOptions", "new"),
    ("Command", "new"),
    ("TcpStream", "connect"),
    ("TcpListener", "bind"),
    ("UdpSocket", "bind"),
    ("UnixStream", "connect"),
    ("UnixListener", "bind"),
];
/// Macros that write to the process's streams.
const IO_MACROS: [&str; 5] = ["print", "println", "eprint", "eprintln", "dbg"];
/// `.<method>()` calls that block with no deadline.
const BLOCKING_METHODS: [&str; 4] = ["recv", "join", "wait", "lock"];
/// `thread::<fn>(` calls that block or start a thread.
const BLOCKING_THREAD: [&str; 3] = ["spawn", "park", "scope"];

/// One site in a file: its line, what it is, and whether it is in test code and in sans-IO code.
type FileSite = (usize, What, bool, bool);

/// The one table of the statics D170 C3 and D173 allow (the foreign heap, the profile ring, the signal
/// handler), by crate and name: blaze-lint's `ALLOWED_STATICS`, which `process-static` reads.
/// The column reads the same rows from that file's tokens, so the two allow the same statics.
const ALLOWED_TABLE: &str = "crates/blaze-lint/src/allowed.rs";

/// The allowed statics' names, by crate (the crate as [`count_sans`] names it, its directory
/// under `crates/`).
type AllowedStatics = BTreeMap<String, BTreeSet<String>>;

/// The rows of [`ALLOWED_TABLE`]'s text: each `AllowedStatic { krate: "..", name: "..", .. }`.
/// A row with no `krate` or `name` string, or a second row of one crate and name, is refused.
fn parse_allowed(text: &str) -> Result<AllowedStatics, SansError> {
    let toks = lex(text);
    let mut out = AllowedStatics::new();
    // Bounded: one turn per token.
    for (at, tok) in toks.iter().enumerate() {
        if tok.word() != Some("AllowedStatic")
            || kind_at(&toks, at + 1) != Some(TokenKind::OpenBrace)
            || (at >= 1 && word_at(&toks, at - 1) == Some("struct"))
        {
            continue;
        }
        let Some(close) = matching(&toks, at + 1) else {
            return Err(SansError::NotAnAllowedRow { line: tok.line });
        };
        let body = &toks[at + 2..close];
        let field = |name: &str| -> Option<String> {
            body.windows(3).find_map(|w| {
                let is = w[0].word() == Some(name) && w[1].kind == TokenKind::Colon;
                let text = match w[2].kind {
                    TokenKind::Literal { kind: blaze_lex::LiteralKind::Str { terminated: true }, .. } => w[2].text.get(1..w[2].text.len() - 1),
                    _ => None,
                };
                if is { text.map(str::to_owned) } else { None }
            })
        };
        let (Some(krate), Some(name)) = (field("krate"), field("name")) else {
            return Err(SansError::NotAnAllowedRow { line: tok.line });
        };
        if !out.entry(krate.clone()).or_default().insert(name.clone()) {
            return Err(SansError::TwoAllowedRows { line: tok.line, krate, name });
        }
    }
    Ok(out)
}

/// The names of `names` the file of `toks` makes its own: declared there (`mod io;`) or
/// imported from anything but `std`, `core` or `alloc` (`use crate::fs;`), as blaze-lint's
/// `OwnHeads::read` reads them. A bare head of one (`fs::step(`) is the crate's module there.
fn own_heads<'n>(toks: &[Tok<'_>], names: &[&'n str]) -> BTreeSet<&'n str> {
    let mut out = BTreeSet::new();
    let mut at = 0;
    // Bounded: each turn moves `at` forward (a `use` to its `;`).
    while at < toks.len() {
        match toks[at].word() {
            Some("mod") => {
                if let Some(name) = word_at(toks, at + 1).and_then(|w| names.iter().find(|n| **n == w)) {
                    out.insert(*name);
                }
                at += 1;
            }
            Some("use") => {
                let from_std = word_at(toks, at + 1).is_some_and(|w| matches!(w, "std" | "core" | "alloc"))
                    || (sep_at(toks, at + 1) && word_at(toks, at + 3).is_some_and(|w| matches!(w, "std" | "core" | "alloc")));
                let end = (at + 1..toks.len()).find(|n| toks[*n].kind == TokenKind::Semi).unwrap_or(toks.len());
                for n in at + 1..end {
                    let leaf = matches!(kind_at(toks, n + 1), Some(TokenKind::Comma | TokenKind::CloseBrace | TokenKind::Semi))
                        || word_at(toks, n + 1) == Some("as");
                    if !from_std
                        && leaf
                        && let Some(name) = word_at(toks, n).and_then(|w| names.iter().find(|m| **m == w))
                    {
                        out.insert(*name);
                    }
                }
                at = end + 1;
            }
            _ => at += 1,
        }
    }
    out
}

/// The name a `static` at `at` declares (`static NAME` or `static mut NAME`).
fn static_name<'t>(toks: &[Tok<'t>], at: usize) -> Option<&'t str> {
    if word_at(toks, at + 1) == Some("mut") { word_at(toks, at + 2) } else { word_at(toks, at + 1) }
}

/// The lock types of D179's constructors: blaze-lint's `LOCK_TYPES` (engine.rs), kept equal by
/// hand.
const LOCK_TYPES: [&str; 5] = ["Lock", "ReadLock", "RwLock", "RankedLock", "SharedMap"];

/// `<type>::<ctor>` is one of D179's constructors (blaze-lint's `lock_ctor`).
fn lock_constructs(ty: &str, ctor: &str) -> bool {
    match ty {
        "Lock" => matches!(ctor, "new" | "unrecorded" | "probed"),
        "ReadLock" => matches!(ctor, "new" | "unrecorded"),
        "RwLock" | "RankedLock" => ctor == "new",
        "SharedMap" => ctor == "new_in",
        _ => false,
    }
}

/// What a file's tokens say about its locks before the sites are read: each
/// `#[blaze::lock_exception(..)]`'s reach, and each `use` (to its `;`), as token ranges with
/// both ends in.
struct LockSpans {
    reasons: Vec<(usize, usize)>,
    uses: Vec<(usize, usize)>,
}

fn lock_spans(toks: &[Tok<'_>]) -> LockSpans {
    let mut out = LockSpans { reasons: Vec::new(), uses: Vec::new() };
    // Bounded: one turn per token.
    for (at, tok) in toks.iter().enumerate() {
        if tok.kind == TokenKind::Pound
            && let Some(attr) = attribute(toks, at)
            && !attr.inner
            && attr.to >= attr.from + 4
            && word_at(toks, attr.from) == Some("blaze")
            && sep_at(toks, attr.from + 1)
            && word_at(toks, attr.from + 3) == Some("lock_exception")
        {
            out.reasons.push((at, reason_end(toks, attr.after)));
        }
        if tok.word() == Some("use") && tok.kind == TokenKind::Ident {
            let end = (at + 1..toks.len()).find(|n| toks[*n].kind == TokenKind::Semi).unwrap_or(toks.len().saturating_sub(1));
            out.uses.push((at, end));
        }
    }
    out
}

/// The last token a `lock_exception` whose `]` is just before `from` covers: blaze-lint's
/// `reason_end`, kept equal by hand. The first `;` or `,` outside brackets, the `}` that brings
/// the depth back to 0 unless `else` follows it, or the token before a bracket that closes
/// around it; the file's last token when none.
fn reason_end(toks: &[Tok<'_>], from: usize) -> usize {
    let mut depth: usize = 0;
    // Bounded: one turn per token, to the end at most.
    for (at, tok) in toks.iter().enumerate().skip(from) {
        if opens(tok.kind) {
            depth += 1;
        } else if closes(tok.kind) {
            if depth == 0 {
                return at.saturating_sub(1);
            }
            depth -= 1;
            if tok.kind == TokenKind::CloseBrace && depth == 0 && word_at(toks, at + 1) != Some("else") {
                return at;
            }
        } else if matches!(tok.kind, TokenKind::Semi | TokenKind::Comma) && depth == 0 {
            return at;
        }
    }
    toks.len().saturating_sub(1)
}

/// The word at `at`, one of [`LOCK_TYPES`], starts `<type>::<ctor>` (a turbofish between
/// allowed, `Lock::<T>::new`) naming one of D179's constructors, not after a `.`, not in a
/// `use`, and not an associated item of a type (`Outer::Lock::new`: a path before it is of
/// lowercase modules).
fn lock_ctor_at(toks: &[Tok<'_>], at: usize, uses: &[(usize, usize)]) -> bool {
    let Some(ty) = word_at(toks, at) else { return false };
    let after_dot = at >= 1 && kind_at(toks, at - 1) == Some(TokenKind::Dot) && !(at >= 2 && kind_at(toks, at - 2) == Some(TokenKind::Dot));
    let under_type = at >= 2
        && sep_at(toks, at - 2)
        && !(at >= 3 && word_at(toks, at - 3).is_some_and(|m| m.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b == b'_')));
    if after_dot || under_type || !sep_at(toks, at + 1) || uses.iter().any(|(from, to)| *from <= at && at <= *to) {
        return false;
    }
    let mut ctor = at + 3;
    if kind_at(toks, ctor) == Some(TokenKind::Lt) {
        let mut depth: usize = 0;
        let mut close = None;
        // Bounded: one turn per token, to the end at most.
        for (n, tok) in toks.iter().enumerate().skip(ctor) {
            match tok.kind {
                TokenKind::Lt | TokenKind::OpenParen | TokenKind::OpenBracket => depth += 1,
                TokenKind::Gt if n >= 1 && kind_at(toks, n - 1) == Some(TokenKind::Minus) => {}
                TokenKind::Gt | TokenKind::CloseParen | TokenKind::CloseBracket => {
                    let Some(less) = depth.checked_sub(1) else { return false };
                    depth = less;
                    if depth == 0 {
                        close = Some(n);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { return false };
        if !sep_at(toks, close + 1) {
            return false;
        }
        ctor = close + 3;
    }
    word_at(toks, ctor).is_some_and(|name| lock_constructs(ty, name))
}

/// Each site of the marked tokens `toks`, in a crate whose allowed statics are `allowed`.
fn sites_of(toks: &[Tok<'_>], allowed: &BTreeSet<String>) -> Vec<FileSite> {
    let own = own_heads(toks, &["fs"]);
    let allows = |name: Option<&str>| name.is_some_and(|it| allowed.contains(it));
    let locks = lock_spans(toks);
    let mut out = Vec::new();
    // Bounded: one turn per token.
    for (at, tok) in toks.iter().enumerate() {
        let Some(word) = tok.word() else { continue };
        let mut push = |what: What| out.push((tok.line, what, tok.test, tok.core));
        if LOCK_TYPES.contains(&word) && lock_ctor_at(toks, at, &locks.uses) {
            push(What::Lock);
            if !locks.reasons.iter().any(|(from, to)| *from <= at && at <= *to) {
                push(What::LockNoReason);
            }
        }
        let prev_sep = at >= 2 && sep_at(toks, at - 2);
        let prev_word = if prev_sep && at >= 3 { word_at(toks, at - 3) } else { None };
        // The first segment of a path: no `::` before it, or a leading one (`::std::x`).
        let first = !prev_sep || at < 3 || !(word_at(toks, at - 3).is_some() || kind_at(toks, at - 3) == Some(TokenKind::Gt));
        let next_sep = sep_at(toks, at + 1);
        let next_open = kind_at(toks, at + 1) == Some(TokenKind::OpenParen);
        let after_dot = at >= 1 && kind_at(toks, at - 1) == Some(TokenKind::Dot) && !(at >= 2 && kind_at(toks, at - 2) == Some(TokenKind::Dot));
        let after_def = at >= 1 && word_at(toks, at - 1).is_some_and(|it| DEFINING.contains(&it));
        let macro_call = kind_at(toks, at + 1) == Some(TokenKind::Bang) && kind_at(toks, at + 2).is_some_and(opens);
        let extern_crate = at >= 2 && word_at(toks, at - 1) == Some("crate") && word_at(toks, at - 2) == Some("extern");
        match word {
            "std" | "alloc" if first && (next_sep || extern_crate) => {
                push(if word == "std" { What::Std } else { What::Alloc });
                if word == "std" && extern_crate {
                    push(What::ExternStd);
                }
                if word == "std" && next_sep && word_at(toks, at + 3).is_some_and(|it| IO_MODULES.contains(&it)) {
                    push(What::Io);
                }
            }
            "Global" if !after_def => push(What::Global),
            "global" if next_open && matches!(prev_word, Some("Alloc" | "SharedAlloc")) => push(What::AllocGlobal),
            "thread_local" if kind_at(toks, at + 1) == Some(TokenKind::Bang) => {
                // Not a site when every `static` it declares is an allowed one of the crate.
                let body = matching(toks, at + 2).map(|close| at + 3..close);
                let names: Option<Vec<Option<&str>>> =
                    body.map(|body| body.filter(|n| word_at(toks, *n) == Some("static")).map(|n| static_name(toks, n)).collect());
                let all_allowed = names.is_some_and(|names| !names.is_empty() && names.iter().all(|name| allows(*name)));
                if !all_allowed {
                    push(What::Static);
                }
            }
            "static" if tok.kind == TokenKind::Ident && allows(static_name(toks, at)) => {}
            "static" if tok.kind == TokenKind::Ident => {
                if word_at(toks, at + 1) == Some("mut") {
                    push(What::Static);
                } else if word_at(toks, at + 1).is_some() && kind_at(toks, at + 2) == Some(TokenKind::Colon) && !sep_at(toks, at + 2) {
                    let mut depth: usize = 0;
                    let mut state = false;
                    // Bounded: the type's tokens, to its `=` or `;`.
                    for it in toks.iter().skip(at + 3) {
                        match it.kind {
                            TokenKind::Eq | TokenKind::Semi if depth == 0 => break,
                            kind if opens(kind) => depth += 1,
                            kind if closes(kind) => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                        if let Some(w) = it.word() {
                            state |= stateful(w);
                        }
                    }
                    if state {
                        push(What::Static);
                    }
                }
            }
            _ => {}
        }
        if IO_CRATES.contains(&word) && first && next_sep {
            let called = word_at(toks, at + 3);
            let reach_files_it = CALLS.iter().any(|it| it.prev == Some(word) && Some(it.word) == called);
            if !reach_files_it {
                push(What::Io);
            }
        }
        if next_open && prev_word.is_some_and(|p| IMPLICIT_CALLS.contains(&(p, word))) {
            push(What::Implicit);
        }
        if macro_call && !after_dot && !after_def && IMPLICIT_MACROS.contains(&word) {
            push(What::Implicit);
        }
        if after_dot && next_open && IMPLICIT_METHODS.contains(&word) {
            push(What::Implicit);
        }
        if macro_call && !after_dot && !after_def && IO_MACROS.contains(&word) {
            push(What::Io);
        }
        if next_open && prev_word.is_some_and(|p| IO_CALLS.contains(&(p, word))) {
            push(What::Io);
        }
        // `fs::read(..)` with `fs` the path's first segment (`use std::fs;`): `std::fs::read`
        // is the `std::fs` path's already. Not std's in a file that makes `fs` its own
        // (`mod fs;`, `use crate::fs;`: [`own_heads`]).
        if next_open && prev_word == Some("fs") && at >= 3 && !(at >= 5 && sep_at(toks, at - 5)) && !own.contains("fs") {
            push(What::Io);
        }
        if after_dot && next_open && kind_at(toks, at + 2) == Some(TokenKind::CloseParen) && BLOCKING_METHODS.contains(&word) {
            push(What::Blocking);
        }
        if next_open && prev_word == Some("thread") && BLOCKING_THREAD.contains(&word) {
            push(What::Blocking);
        }
        if next_open && word == "block_on" && !after_def {
            push(What::Blocking);
        }
        // The reach ratchet's calls (reach.rs), and `Os::process()` outside a `fn main`.
        if next_open && !(prev_word.is_none() && (after_dot || after_def)) {
            if prev_word == Some("Os") && word == "process" {
                if !tok.main {
                    push(What::Io);
                }
            } else if let Some(call) = CALLS.iter().find(|it| it.word == word && (it.prev.is_none() || it.prev == prev_word)) {
                push(match call.column {
                    ReachColumn::Clock => What::Clock,
                    ReachColumn::Env => What::Env,
                    ReachColumn::Random => What::Random,
                    ReachColumn::Exit | ReachColumn::World | ReachColumn::TestRoot => What::Io,
                });
            }
        }
    }
    out
}

/// A file read: its top-level marks and its sites.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct FileScan {
    pub(super) marks: Marks,
    pub(super) sites: Vec<FileSite>,
    /// The allowed statics of its crate it declares outside test code, by name, once each time.
    pub(super) declared: Vec<String>,
    /// The `#[macro_export]` `macro_rules!` it defines, by name, test code included.
    pub(super) exported: Vec<String>,
    /// Each `crate::<name>!` and `$crate::<name>!` it writes, test code included: its line and
    /// the name.
    pub(super) macro_paths: Vec<(usize, String)>,
}

/// The `#[macro_export]` macros the tokens define, and each `crate::<name>!` or
/// `$crate::<name>!` they write (a macro called by path from its own crate; an exported macro
/// lives at the crate's root, so only a path of one segment after `crate` can name one).
fn macro_facts(toks: &[Tok<'_>]) -> (Vec<String>, Vec<(usize, String)>) {
    let (mut exported, mut paths) = (Vec::new(), Vec::new());
    // Bounded: one turn per token.
    for (at, tok) in toks.iter().enumerate() {
        if tok.kind == TokenKind::Pound
            && let Some(attr) = attribute(toks, at)
            && !attr.inner
            && word_at(toks, attr.from) == Some("macro_export")
        {
            let mut next = attr.after;
            // Bounded: each turn moves past one attribute.
            while kind_at(toks, next) == Some(TokenKind::Pound) {
                match attribute(toks, next) {
                    Some(more) => next = more.after,
                    None => break,
                }
            }
            if word_at(toks, next) == Some("macro_rules")
                && kind_at(toks, next + 1) == Some(TokenKind::Bang)
                && let Some(name) = word_at(toks, next + 2)
            {
                exported.push(name.to_owned());
            }
        }
        if tok.word() == Some("crate")
            && sep_at(toks, at + 1)
            && kind_at(toks, at + 4) == Some(TokenKind::Bang)
            && let Some(name) = word_at(toks, at + 3)
        {
            paths.push((tok.line, name.to_owned()));
        }
    }
    (exported, paths)
}

/// Reads one file's text. `test` and `core` say the walk already knows the whole file is test
/// code, or sans-IO code (its crate's root, or a module above it, says so). A crate with no
/// allowed statics: [`scan_tokens_in`] for one that has them.
pub(super) fn scan_tokens(text: &str, test: bool, core: bool) -> FileScan {
    scan_tokens_in(text, test, core, &BTreeSet::new())
}

/// [`scan_tokens`] in a crate whose allowed statics ([`ALLOWED_TABLE`]) are `allowed`.
fn scan_tokens_in(text: &str, test: bool, core: bool, allowed: &BTreeSet<String>) -> FileScan {
    let mut toks = lex(text);
    for tok in &mut toks {
        tok.test |= test;
        tok.core |= core;
    }
    let marks = mark(&mut toks);
    let declared = (0..toks.len())
        .filter(|at| !toks[*at].test && word_at(&toks, *at) == Some("static"))
        .filter_map(|at| static_name(&toks, at))
        .filter(|name| allowed.contains(*name))
        .map(str::to_owned)
        .collect();
    let (exported, macro_paths) = macro_facts(&toks);
    FileScan { marks, sites: sites_of(&toks, allowed), declared, exported, macro_paths }
}

/// What a file's top-level inner attributes say, without reading its sites.
fn marks_of(text: &str) -> Marks {
    mark(&mut lex(text))
}

/// One site in the tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SansSite {
    /// Relative to the root.
    path: String,
    line: usize,
    col: Col,
}

/// A crate as the gate reads it.
#[derive(Debug, Default)]
struct CrateFacts {
    /// Its root, from the root, and what its inner attributes say; `None` for a crate with no
    /// root (files under crates/ with no package).
    root: Option<(String, Marks)>,
    /// Its package name (for thirdparty.rs's rows).
    package: Option<String>,
    sites: Vec<SansSite>,
    /// Each allowed static of the crate ([`ALLOWED_TABLE`]) its code declares, with how often.
    declared: BTreeMap<String, usize>,
    /// Its `extern crate std` outside test code, as `path:line`.
    extern_std: Vec<String>,
    /// The `#[macro_export]` macros its library's files define ([`macro_facts`]).
    exported: BTreeSet<String>,
    /// Each `crate::<name>!` or `$crate::<name>!` its library's files write: `path`, line, name.
    macro_paths: Vec<(String, usize, String)>,
}

impl CrateFacts {
    /// A file's macros, when it is one of the library's own files (its unit tests in them
    /// included): an integration test, a bench, an example, a binary or a build script is a
    /// crate of its own, whose `crate::` is not the library's. The library's files are those
    /// under its `src/` but `src/bin/`: a `src/tests.rs` or `src/tests/` is a module of the
    /// library, so this is not ambient.rs's `test_path`.
    fn add_macros(&mut self, path: &str, exported: Vec<String>, paths: Vec<(usize, String)>) {
        let library = (path.starts_with("src/") || path.contains("/src/")) && !path.contains("/src/bin/") && !path.starts_with("src/bin/");
        if !library {
            return;
        }
        self.exported.extend(exported);
        for (line, name) in paths {
            self.macro_paths.push((path.to_owned(), line, name));
        }
    }

    fn row(&self) -> SansRow {
        let mut row = [0; WIDTH];
        for it in &self.sites {
            row[it.col.at()] += 1;
        }
        row
    }
}

type SansCounts = BTreeMap<String, CrateFacts>;

/// The `name` of a manifest's `[package]`.
fn package_name(text: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.split('\n') {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if in_package && key.trim() == "name" {
            let value = value.split('#').next().unwrap_or(value).trim();
            return value.strip_prefix('"').and_then(|it| it.strip_suffix('"')).map(str::to_owned);
        }
    }
    None
}

fn read_lossy(root: &Path, path: &str) -> Result<String, TreeError> {
    let bytes = std::fs::read(root.join(path)).map_err(|e| TreeError::Unreadable { path: path.to_owned(), why: e.to_string() })?;
    Ok(universal_newlines(String::from_utf8_lossy(&bytes).into_owned()))
}

fn count_sans(root: &Path, allowed: &AllowedStatics) -> Result<SansCounts, TreeError> {
    let none = BTreeSet::new();
    let Walked { scanned, skipped, children, packages, .. } = walk(root)?;
    let mut out: SansCounts = BTreeMap::new();
    // Each package's root and what it says (a package under `tests` or `benches` is a fixture).
    for parts in packages.iter().filter(|it| !test_path(it)) {
        let Some(path) = crate_root(root, parts)? else { continue };
        let krate = parts[1..parts.len() - 1].join("/");
        let marks = marks_of(&read_lossy(root, &path)?);
        let facts = out.entry(krate).or_default();
        if !marks.no_std {
            facts.sites.push(SansSite { path: path.clone(), line: 1, col: Col::NoStd });
        }
        if !marks.sans_io && !marks.driver {
            facts.sites.push(SansSite { path: path.clone(), line: 1, col: Col::Unmarked });
        }
        facts.package = package_name(&read_lossy(root, &parts.join("/"))?);
        facts.root = Some((path, marks));
    }
    let mut manifests: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    let crate_of_file: Vec<(String, String)> =
        scanned.iter().map(|(parts, _)| (parts.join("/"), crate_of(root, parts, &mut manifests))).collect();
    // The files marked sans-IO: each whose crate's root says so, each whose own top says so, and
    // every module such a file declares, at any depth.
    let mut core: BTreeSet<String> = BTreeSet::new();
    for (path, krate) in &crate_of_file {
        let root_says = out.get(krate).and_then(|it| it.root.as_ref()).is_some_and(|(_, marks)| marks.sans_io);
        if root_says {
            core.insert(path.clone());
        } else if !skipped.contains(path) {
            // Lexed only when its text names the attribute.
            let text = read_lossy(root, path)?;
            if text.contains("sans_io") && marks_of(&text).sans_io {
                core.insert(path.clone());
            }
        }
    }
    let mut todo: Vec<String> = core.iter().cloned().collect();
    // Bounded: a file enters `core` once, and only a file that entered is taken again.
    while let Some(path) = todo.pop() {
        for child in children.get(&path).into_iter().flatten() {
            if core.insert(child.clone()) {
                todo.push(child.clone());
            }
        }
    }
    for (path, krate) in crate_of_file {
        let test = skipped.contains(&path);
        let found = scan_tokens_in(&read_lossy(root, &path)?, test, core.contains(&path), allowed.get(&krate).unwrap_or(&none));
        let facts = out.entry(krate).or_default();
        for name in found.declared {
            *facts.declared.entry(name).or_default() += 1;
        }
        facts.add_macros(&path, found.exported, found.macro_paths);
        for (line, what, in_test, in_core) in found.sites {
            if what == What::ExternStd && !in_test {
                facts.extern_std.push(format!("{path}:{line}"));
            }
            let col = if in_test { what.in_tests() } else { what.shipped() };
            if let Some(col) = col {
                facts.sites.push(SansSite { path: path.clone(), line, col });
            }
            if in_core && !in_test && what.breaks() {
                facts.sites.push(SansSite { path: path.clone(), line, col: Col::Breaks });
            }
        }
    }
    Ok(out)
}

/// thirdparty.rs's list of the workspace crates the shipped binaries reach (the one home of the
/// manifests' graph; its `check` holds the list to the manifests).
const REACHED_LIST: &str = "tools/ratchets/reached.txt";

/// The crates D170 and D173 exempt from `bare`, by crate (its directory under `crates/`), each
/// with why. Nothing else is skipped: a reached crate not here is held to the rule.
const EXEMPT: [(&str, &str); 4] = [
    ("blaze-alloc-count", "the allocation counter: a test tool that never ships in the binary (D170, exempt from C1)"),
    ("blaze-check", "the model checker: a test tool that never ships in the binary (D170, exempt from C1)"),
    ("blaze-http", "the driver that hosts the vendored cryptography and QUIC (rustls, ring, quinn-proto with bytes), not edited without the owner's word (D170, D173)"),
    ("blaze-wasi", "hosts Wasmtime for proc macros and build scripts until decision D175's switch (WebAssembly is a bridge)"),
];

/// One reached crate: its direct third-party packages' count and the crates it names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ReachedRow {
    third_party: usize,
    deps: Vec<String>,
}

/// A directory of the reached list as the gate names crates: under `crates/`, without it.
fn crate_key(dir: &str) -> String {
    dir.strip_prefix("crates/").unwrap_or(dir).to_owned()
}

/// The reached list's rows `<dir>\t<third_party>\t<deps or ->`, by crate.
fn parse_reached(text: &str) -> Result<BTreeMap<String, ReachedRow>, SansError> {
    let mut out = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let row = match line.split('\t').collect::<Vec<&str>>().as_slice() {
            [dir, third, deps] if !dir.is_empty() => third.parse::<usize>().ok().map(|third_party| {
                let deps = if *deps == "-" { Vec::new() } else { deps.split(',').map(crate_key).collect() };
                (crate_key(dir), ReachedRow { third_party, deps })
            }),
            _ => None,
        };
        let Some((krate, row)) = row else {
            return Err(SansError::NotARow { list: REACHED_LIST, line: at + 1, text: line.to_owned() });
        };
        if out.insert(krate.clone(), row).is_some() {
            return Err(SansError::TwoRows { list: REACHED_LIST, line: at + 1, krate });
        }
    }
    Ok(out)
}

/// Why a reached crate is not bare, the first reason found; `None` when it is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NotBare {
    NoRoot,
    NotNoStd,
    Alloc { first: String },
    ExternStd { at: String },
    ThirdParty { count: usize },
    Dep { krate: String },
    /// A cycle of normal and build dependencies, which cargo refuses.
    Cycle,
}

impl fmt::Display for NotBare {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotBare::NoRoot => write!(f, "no crate root under crates/"),
            NotBare::NotNoStd => write!(f, "its root is not `#![no_std]`"),
            NotBare::Alloc { first } => write!(f, "it names `alloc` ({first})"),
            NotBare::ExternStd { at } => write!(f, "`extern crate std` outside cfg(test) ({at})"),
            NotBare::ThirdParty { count } => write!(f, "{count} third-party packages named directly"),
            NotBare::Dep { krate } => write!(f, "it names {krate}, which is not bare"),
            NotBare::Cycle => write!(f, "a cycle of dependencies through it"),
        }
    }
}

/// The verdict on each reached crate: `Ok(())` bare, or why not; exempt crates are not in it.
fn bare_crates(counts: &SansCounts, reached: &BTreeMap<String, ReachedRow>) -> BTreeMap<String, Result<(), NotBare>> {
    fn judge(
        krate: &str,
        counts: &SansCounts,
        reached: &BTreeMap<String, ReachedRow>,
        out: &mut BTreeMap<String, Result<(), NotBare>>,
        open: &mut BTreeSet<String>,
    ) -> bool {
        if EXEMPT.iter().any(|(name, _)| *name == krate) {
            return true;
        }
        if let Some(done) = out.get(krate) {
            return done.is_ok();
        }
        if !open.insert(krate.to_owned()) {
            return false;
        }
        let facts = counts.get(krate);
        let row = reached.get(krate).cloned().unwrap_or_default();
        let first_alloc = facts.and_then(|it| it.sites.iter().find(|s| s.col == Col::Alloc)).map(|s| format!("{}:{}", s.path, s.line));
        let mut verdict = match facts.and_then(|it| it.root.as_ref()) {
            None => Err(NotBare::NoRoot),
            Some((_, marks)) if !marks.no_std => Err(NotBare::NotNoStd),
            Some(_) => match (first_alloc, facts.and_then(|it| it.extern_std.first())) {
                (Some(first), _) => Err(NotBare::Alloc { first }),
                (None, Some(at)) => Err(NotBare::ExternStd { at: at.clone() }),
                (None, None) if row.third_party > 0 => Err(NotBare::ThirdParty { count: row.third_party }),
                (None, None) => Ok(()),
            },
        };
        for dep in &row.deps {
            let ok = judge(dep, counts, reached, out, open);
            if verdict.is_ok() && !ok {
                // A dependency judged and refused is named; one still open is a cycle.
                verdict = Err(if out.contains_key(dep) { NotBare::Dep { krate: dep.clone() } } else { NotBare::Cycle });
            }
        }
        open.remove(krate);
        let ok = verdict.is_ok();
        out.insert(krate.to_owned(), verdict);
        ok
    }
    let mut out = BTreeMap::new();
    let mut open = BTreeSet::new();
    for krate in reached.keys() {
        judge(krate, counts, reached, &mut out, &mut open);
    }
    out
}

/// Puts each reached crate that is not bare in the `bare` column, at its root (line 1).
fn add_bare(counts: &mut SansCounts, verdicts: &BTreeMap<String, Result<(), NotBare>>) {
    for (krate, verdict) in verdicts {
        if verdict.is_err() {
            let facts = counts.entry(krate.clone()).or_default();
            let path = facts.root.as_ref().map_or_else(|| format!("crates/{krate}"), |(path, _)| path.clone());
            facts.sites.push(SansSite { path, line: 1, col: Col::Bare });
        }
    }
}

/// The three numbers the owner asks for: crates reached, `no_std`, `no_std` without `alloc`.
fn bare_summary(counts: &SansCounts, verdicts: &BTreeMap<String, Result<(), NotBare>>) -> String {
    let reached = verdicts.len();
    let no_std = verdicts.keys().filter(|k| counts.get(*k).and_then(|it| it.root.as_ref()).is_some_and(|(_, m)| m.no_std)).count();
    let bare = verdicts.values().filter(|v| v.is_ok()).count();
    format!("{reached} crates reached (and {} exempt), {no_std} no_std, {bare} no_std without alloc", EXEMPT.len())
}

/// The list, the done list or the third-party list read wrong, or the base not read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SansError {
    /// A line (from 1) of `list` that is not a row, a comment or blank.
    NotARow { list: &'static str, line: usize, text: String },
    /// A crate's second row in `list`, at its line (from 1).
    TwoRows { list: &'static str, line: usize, krate: String },
    /// A crate of the done list that is no crate with a root, at its line (from 1).
    NotACrate { line: usize, krate: String },
    /// The base's list does not read.
    AtBase { base: String, error: Box<SansError> },
    Base(BaseError),
    /// An `AllowedStatic { .. }` of [`ALLOWED_TABLE`], at its line (from 1), with no `krate`
    /// or `name` string, or not closed.
    NotAnAllowedRow { line: usize },
    /// A second row of one crate and name in [`ALLOWED_TABLE`], at its line (from 1).
    TwoAllowedRows { line: usize, krate: String, name: String },
    /// A row of [`ALLOWED_TABLE`] whose crate's code declares that static `times` times, not
    /// once: the row allows nothing that is there, or more than the one static it names.
    AllowedNotDeclared { krate: String, name: String, times: usize },
}

impl fmt::Display for SansError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SansError::NotARow { list, line, text } => write!(f, "{list}:{line}: {text:?} is not a row of the columns its header names"),
            SansError::TwoRows { list, line, krate } => write!(f, "{list}:{line}: `{krate}` has two rows"),
            SansError::NotACrate { line, krate } => {
                write!(f, "{DONE_LIST}:{line}: `{krate}` is no crate under crates/ with a root (a directory with a Cargo.toml and a lib or bin)")
            }
            SansError::AtBase { base, error } => write!(f, "at the base {base}: {error}"),
            SansError::Base(error) => write!(f, "{error}"),
            SansError::NotAnAllowedRow { line } => {
                write!(f, "{ALLOWED_TABLE}:{line}: an `AllowedStatic {{ .. }}` with no `krate: \"..\"` and `name: \"..\"`")
            }
            SansError::TwoAllowedRows { line, krate, name } => write!(f, "{ALLOWED_TABLE}:{line}: a second row of {krate}'s `{name}`"),
            SansError::AllowedNotDeclared { krate, name, times } => write!(
                f,
                "{ALLOWED_TABLE}: {krate}'s `{name}` is declared {times} times outside test code, not once: a row names exactly one static (D170 C3)"
            ),
        }
    }
}

impl std::error::Error for SansError {}

/// Rows of `n` numbers after the crate, from `text`.
fn parse_numbers<const N: usize>(text: &str, list: &'static str) -> Result<BTreeMap<String, [usize; N]>, SansError> {
    let mut rows = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let row = match fields.as_slice() {
            [krate, rest @ ..] if !krate.is_empty() && rest.len() == N => {
                let mut n = [0; N];
                let read = rest.iter().zip(&mut n).all(|(field, slot)| field.parse::<usize>().map(|it| *slot = it).is_ok());
                read.then_some((*krate, n))
            }
            _ => None,
        };
        let Some((krate, n)) = row else {
            return Err(SansError::NotARow { list, line: at + 1, text: line.to_owned() });
        };
        if rows.insert(krate.to_owned(), n).is_some() {
            return Err(SansError::TwoRows { list, line: at + 1, krate: krate.to_owned() });
        }
    }
    Ok(rows)
}

fn parse_sans_rows(text: &str) -> Result<BTreeMap<String, SansRow>, SansError> {
    parse_numbers::<WIDTH>(text, SANS_LIST)
}

/// The done list's crates, each with its line (from 1).
fn parse_done(text: &str) -> Vec<(usize, String)> {
    text.split('\n')
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|(at, line)| (at + 1, line.trim().to_owned()))
        .collect()
}

fn row_line(krate: &str, row: &SansRow) -> String {
    let cells: Vec<String> = row.iter().map(usize::to_string).collect();
    format!("{krate}\t{}\n", cells.join("\t"))
}

/// The list as the first write writes it: each crate with a site.
fn render_sans(counts: &SansCounts) -> String {
    let mut out = SANS_HEADER.to_owned();
    for (krate, facts) in counts {
        let row = facts.row();
        if row != [0; WIDTH] {
            out.push_str(&row_line(krate, &row));
        }
    }
    out
}

/// Each row a later write keeps: the lower of the row and the count, column by column; a
/// crate with no row stays without one (held at 0); a row at 0 everywhere goes.
fn held_sans(counts: &SansCounts, rows: &BTreeMap<String, SansRow>) -> BTreeMap<String, SansRow> {
    let mut out = BTreeMap::new();
    for (krate, row) in rows {
        let now = counts.get(krate).map_or([0; WIDTH], CrateFacts::row);
        let mut kept = [0; WIDTH];
        for (at, slot) in kept.iter_mut().enumerate() {
            *slot = row[at].min(now[at]);
        }
        if kept != [0; WIDTH] {
            out.insert(krate.clone(), kept);
        }
    }
    out
}

fn render_sans_rows(rows: &BTreeMap<String, SansRow>, notes: &BTreeMap<String, Vec<String>>) -> String {
    let mut out = SANS_HEADER.to_owned();
    for (krate, row) in rows {
        for note in notes.get(krate).into_iter().flatten() {
            out.push_str(note);
            out.push('\n');
        }
        out.push_str(&row_line(krate, row));
    }
    out
}

/// The first three sites of `col` in `facts`, the ones on lines added since the base first.
fn first_sites(facts: &CrateFacts, col: Col, added: Option<&BTreeMap<String, BTreeSet<usize>>>) -> String {
    let of_col: Vec<&SansSite> = facts.sites.iter().filter(|it| it.col == col).collect();
    let is_new = |it: &&SansSite| added.is_some_and(|added| added.get(&it.path).is_some_and(|lines| lines.contains(&it.line)));
    let (mut shown, rest): (Vec<&SansSite>, Vec<&SansSite>) = of_col.iter().copied().partition(|it| is_new(it));
    let new = shown.len();
    shown.extend(rest);
    let named: Vec<String> = shown.iter().take(3).map(|it| format!("{}:{}", it.path, it.line)).collect();
    let more = of_col.len().saturating_sub(3);
    let tail = if more > 0 { format!(" and {more} more") } else { String::new() };
    let new = match added {
        Some(_) => format!("; {new} on lines added since the base"),
        None => String::new(),
    };
    format!("{}{tail}{new}", named.join(", "))
}

/// The lints a done crate's root denies (C7): every one, and `sans_io_core` for a core.
const DONE_LINTS: [&str; 3] = ["std_use", "implicit_allocator", "process_static"];

/// The record of the class [`macro_path_problems`] guards.
const MACRO_PATH_ANNOYANCE: &str = "agents/annoyances/crate-root-tool-attribute-hides-macros.json";

/// Each crate whose root carries a `#![blaze::..]` inner attribute and whose library names one
/// of its own `#[macro_export]` macros by path (`crate::name!`, `$crate::name!`), refused by
/// name. A tool attribute at the root makes rustc read the whole crate as macro-expanded, and
/// then "macro-expanded `macro_export` macros from the current crate cannot be referred to by
/// absolute paths" (blaze-data, blaze-mem, blaze-phf, blaze-sim, blaze-rng).
fn macro_path_problems(counts: &SansCounts) -> Vec<String> {
    let mut out = Vec::new();
    for (krate, facts) in counts {
        let Some((root, Marks { tool: Some(tool), .. })) = &facts.root else { continue };
        let named: Vec<&(String, usize, String)> = facts.macro_paths.iter().filter(|(_, _, name)| facts.exported.contains(name)).collect();
        let Some((_, _, first)) = named.first() else { continue };
        let sites: Vec<String> = named.iter().take(3).map(|(path, line, name)| format!("{path}:{line} `{name}!`")).collect();
        let more = named.len().saturating_sub(3);
        let tail = if more > 0 { format!(" and {more} more") } else { String::new() };
        out.push(format!(
            "`{krate}`: its root {root} says `#![{tool}]`, so rustc reads the crate as macro-expanded and refuses its own exported macros named by path (\"macro-expanded `macro_export` macros from the current crate cannot be referred to by absolute paths\"), named at {}{tail}: call it by its bare name after its definition (`{first}!`, the defining module declared first, `#[macro_use]` where it is a module of its own), or move the caller to an integration test under tests/ ({MACRO_PATH_ANNOYANCE})",
            sites.join(", ")
        ));
    }
    out
}

/// What is wrong: each a refusal, then each a note (a crate under its row).
fn sans_problems(
    counts: &SansCounts,
    rows: &BTreeMap<String, SansRow>,
    base_rows: Option<&BTreeMap<String, SansRow>>,
    added: Option<&BTreeMap<String, BTreeSet<usize>>>,
    done: &[(usize, String)],
    third_party: &BTreeMap<String, [usize; 3]>,
) -> (Vec<String>, Vec<String>) {
    let (mut over, mut under) = (Vec::new(), Vec::<String>::new());
    over.extend(macro_path_problems(counts));
    let empty = CrateFacts::default();
    let crates: BTreeSet<&String> = counts.keys().chain(rows.keys()).collect();
    for krate in crates {
        let facts = counts.get(krate).unwrap_or(&empty);
        let now = facts.row();
        let row = rows.get(krate);
        for col in COLS {
            let (n, held) = (now[col.at()], row.map_or(0, |it| it[col.at()]));
            if n > held {
                let shown = if row.is_some() { format!("its row's {held}") } else { "0, as a crate with no row is held".to_owned() };
                over.push(format!(
                    "`{krate}` has {n} {} (`{}`), over {shown} in {SANS_LIST}: {}; {} (D170 C7)",
                    col.what(),
                    col.name(),
                    first_sites(facts, col, added),
                    col.instead()
                ));
            } else if n < held {
                under.push(format!("`{krate}` has {n} {} (`{}`), under its row's {held}: lower it (command `sansio-write`)", col.what(), col.name()));
            }
        }
    }
    if let Some(base) = base_rows {
        for (krate, row) in rows {
            let was = base.get(krate).copied().unwrap_or([0; WIDTH]);
            for col in COLS {
                let (now, then) = (row[col.at()], was[col.at()]);
                if now > then {
                    over.push(format!("{SANS_LIST}: the row `{krate}`'s `{}` is {now} and was {then} at the base: the list only falls (D170 C7)", col.name()));
                }
            }
        }
    }
    for (line, krate) in done {
        let Some(facts) = counts.get(krate).filter(|it| it.root.is_some()) else {
            over.push(SansError::NotACrate { line: *line, krate: krate.clone() }.to_string());
            continue;
        };
        let now = facts.row();
        // A lock with its reason is D179's named exception, which a done crate may hold; one
        // without is `locks_without_reason`, held at 0 here like every other column.
        for col in COLS.into_iter().filter(|it| *it != Col::Locks) {
            let n = now[col.at()];
            if n > 0 {
                over.push(format!(
                    "`{krate}` is in {DONE_LIST} but has {n} {} (`{}`): {}; {} (D170 C7: a done crate is 0 in every column)",
                    col.what(),
                    col.name(),
                    first_sites(facts, col, added),
                    col.instead()
                ));
            }
        }
        let package = facts.package.as_deref().unwrap_or(krate.as_str());
        if let Some([direct, dev, closure]) = third_party.get(package).copied().filter(|it| *it != [0; 3]) {
            over.push(format!(
                "`{krate}` is in {DONE_LIST} but depends on third-party packages ({direct} direct, {dev} dev, {closure} reached; {THIRD_PARTY_LIST}): replace each (docs/v2/plan/third-party.md)"
            ));
        }
        if let Some((path, marks)) = &facts.root {
            let mut wanted: Vec<&str> = DONE_LINTS.to_vec();
            if marks.sans_io {
                wanted.push("sans_io_core");
            }
            let missing: Vec<String> = wanted.iter().filter(|it| !marks.denied.contains(**it)).map(|it| format!("`blaze::{it}`")).collect();
            if !missing.is_empty() {
                over.push(format!(
                    "`{krate}` is in {DONE_LIST} but its root {path} does not deny {}: add `#![deny({})]` (D170 C7: the lints are at deny in the crate)",
                    missing.join(", "),
                    missing.iter().map(|it| it.trim_matches('`')).collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    (over, under)
}

/// The base's list, or `None` when the base has none (the list begins with the change).
fn base_sans_rows(root: &Path, base: &str) -> Result<Option<BTreeMap<String, SansRow>>, SansError> {
    let root_arg = root.to_string_lossy();
    let dir = root_arg.as_ref();
    let failed = |what: &'static str| move |why: String| SansError::Base(BaseError::Git { base: base.to_owned(), what, why });
    let listed = git(root, &["-C", dir, "ls-tree", "--name-only", base, "--", SANS_LIST]).map_err(failed("list its tree"))?;
    if listed.trim().is_empty() {
        return Ok(None);
    }
    let spec = format!("{base}:{SANS_LIST}");
    let text = git(root, &["-C", dir, "show", spec.as_str()]).map_err(failed("show the list"))?;
    parse_sans_rows(&text).map(Some).map_err(|error| SansError::AtBase { base: base.to_owned(), error: Box::new(error) })
}

fn totals(counts: &SansCounts) -> SansRow {
    let mut row = [0; WIDTH];
    for facts in counts.values() {
        for (sum, n) in row.iter_mut().zip(facts.row()) {
            *sum += n;
        }
    }
    row
}

fn summary(counts: &SansCounts) -> String {
    let row = totals(counts);
    let parts: Vec<String> = COLS.iter().map(|col| format!("{} {}", row[col.at()], col.name())).collect();
    format!("{} in {} crates", parts.join(", "), counts.len())
}

fn root_word(facts: &CrateFacts) -> &'static str {
    match &facts.root {
        Some((_, marks)) if marks.sans_io => "core",
        Some((_, marks)) if marks.driver => "driver",
        Some(_) => "-",
        None => "(no root)",
    }
}

pub(super) fn main_with(command: &str, base: Option<&str>, root: &Path) {
    let allowed = match read_text(&root.join(ALLOWED_TABLE)) {
        Some(text) => parse_allowed(&text).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())),
        None => fail(SCRIPT, &format!("{ALLOWED_TABLE} could not be read: it holds the statics C3 allows, which the statics column reads")),
    };
    let mut counts = count_sans(root, &allowed).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let reached = match read_text(&root.join(REACHED_LIST)) {
        Some(text) => parse_reached(&text).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())),
        None => fail(SCRIPT, &format!("{REACHED_LIST} could not be read: thirdparty's `write` makes it (the crates the binaries reach)")),
    };
    for (krate, _) in EXEMPT {
        if !counts.contains_key(krate) {
            fail(SCRIPT, &format!("the exemption table names {krate}, which is no crate under crates/: take its row out"));
        }
    }
    let verdicts = bare_crates(&counts, &reached);
    add_bare(&mut counts, &verdicts);
    let counts = counts;
    for (krate, names) in &allowed {
        for name in names {
            let times = counts.get(krate).and_then(|facts| facts.declared.get(name)).copied().unwrap_or(0);
            if times != 1 {
                fail(SCRIPT, &SansError::AllowedNotDeclared { krate: krate.clone(), name: name.clone(), times }.to_string());
            }
        }
    }
    let third_party = match read_text(&root.join(THIRD_PARTY_LIST)) {
        Some(text) => parse_numbers::<3>(&text, THIRD_PARTY_LIST).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())),
        None => fail(SCRIPT, &format!("{THIRD_PARTY_LIST} could not be read: the gate reads each crate's third-party row there")),
    };
    if command == "sansio-sites" {
        for (krate, facts) in &counts {
            for it in &facts.sites {
                println!("{krate}\t{}:{}\t{}", it.path, it.line, it.col.name());
            }
        }
        return;
    }
    if command == "sansio-report" {
        let names: Vec<&str> = COLS.iter().map(|it| it.name()).collect();
        println!("crate\troot\t{}\ttp_direct\ttp_dev\ttp_closure\tbare_why", names.join("\t"));
        for (krate, facts) in &counts {
            let package = facts.package.as_deref().unwrap_or(krate.as_str());
            let [direct, dev, closure] = third_party.get(package).copied().unwrap_or([0; 3]);
            let cells: Vec<String> = facts.row().iter().map(usize::to_string).collect();
            let why = match (verdicts.get(krate), EXEMPT.iter().find(|(name, _)| *name == krate.as_str())) {
                (_, Some((_, why))) => format!("exempt: {why}"),
                (Some(Ok(())), None) => "bare".to_owned(),
                (Some(Err(not)), None) => not.to_string(),
                (None, None) => "not reached".to_owned(),
            };
            println!("{krate}\t{}\t{}\t{direct}\t{dev}\t{closure}\t{why}", root_word(facts), cells.join("\t"));
        }
        println!("{SCRIPT}: {}", summary(&counts));
        println!("{SCRIPT}: {}", bare_summary(&counts, &verdicts));
        return;
    }
    let listed = read_text(&root.join(SANS_LIST));
    let rows = match &listed {
        Some(text) => parse_sans_rows(text).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())),
        None if command == "sansio-write" => BTreeMap::new(),
        None => fail(SCRIPT, &format!("{SANS_LIST} could not be read: every crate is held to its row there (command `sansio-write` makes it)")),
    };
    if command == "sansio-write" {
        let text = match &listed {
            Some(text) => render_sans_rows(&held_sans(&counts, &rows), &super::row_notes(text, SANS_HEADER)),
            None => render_sans(&counts),
        };
        if let Err(e) = std::fs::write(root.join(SANS_LIST), text) {
            fail(SCRIPT, &format!("{SANS_LIST} is not written: {e}"));
        }
        println!("{SCRIPT}: wrote {SANS_LIST}: {}", summary(&counts));
        return;
    }
    let done = match read_text(&root.join(DONE_LIST)) {
        Some(text) => parse_done(&text),
        None => fail(SCRIPT, &format!("{DONE_LIST} could not be read: it names the crates that claim C7 (empty is fine, absent is not)")),
    };
    let base_rows = base.map(|base| base_sans_rows(root, base).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())));
    let added = base.map(|base| heap::added_since(root, base).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())));
    if let (Some(base), Some(None)) = (base, &base_rows) {
        println!("{SCRIPT}: {base} has no {SANS_LIST}: the list begins with this change, so no row is compared with the base");
    }
    let (over, under) = sans_problems(&counts, &rows, base_rows.flatten().as_ref(), added.as_ref(), &done, &third_party);
    for note in &under {
        println!("{SCRIPT}: {note}");
    }
    if !over.is_empty() {
        let lines: Vec<String> = over.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
    println!("{SCRIPT}: {}; each crate at or under its row in {SANS_LIST}, {} done crates at 0", summary(&counts), done.len());
    println!("{SCRIPT}: {}", bare_summary(&counts, &verdicts));
}

/// The sites of `text` outside test code, as `(line, what)`.
fn shipped(found: &FileScan) -> Vec<(usize, What)> {
    found.sites.iter().filter(|it| !it.2).map(|it| (it.0, it.1)).collect()
}

/// The cases of the gate's reading, for the script's selftest.
pub(super) fn selftest() {
    // Line by line: 1 `std` and `alloc` heads (a `crate::std` is not one), and `std::fs` is IO
    // too; 2 a comment and strings naming everything; 3 `Global` named and defined; 4 the
    // heap's root; 5 the implicit allocations; 6 an `_in` form, which is not one; 7 statics;
    // 8 a `'static` and a static with no state; 9 the reach calls; 10 IO; 11 a method `now`.
    let text = "use std::fs; use alloc::vec; use crate::std::x; extern crate std;\n\
                // std::x Global Alloc::global() \"Vec::new()\" r#\"thread_local!\"#\n\
                fn g(a: Global) {} struct Global;\n\
                let h = Alloc::global(); let s = SharedAlloc::global();\n\
                let v = Vec::new(); let b = Box::new(1); let f = format!(\"x\"); let t = x.to_string(); let w = vec![1];\n\
                let v = Vec::new_in(alloc);\n\
                thread_local! { static A: u8 = 0; } static mut B: u8 = 0; static C: AtomicU64 = AtomicU64::new(0);\n\
                static D: &'static str = \"\"; static E: Lock<u8> = x;\n\
                let n = Instant::now(); let e = env::var(\"X\"); let r = getrandom(x);\n\
                println!(\"x\"); let f = File::open(p); let p = libc::write(1); let q = fs::read(p); process::exit(1);\n\
                let n = clock.now();\n";
    let found = scan_tokens(text, false, false);
    let want = vec![
        (1, What::Std),
        (1, What::Io),
        (1, What::Alloc),
        (1, What::Std),
        (1, What::ExternStd),
        (3, What::Global),
        (4, What::AllocGlobal),
        (4, What::AllocGlobal),
        (5, What::Implicit),
        (5, What::Implicit),
        (5, What::Implicit),
        (5, What::Implicit),
        (5, What::Implicit),
        (7, What::Static),
        (7, What::Static),
        (7, What::Static),
        (8, What::Static),
        (9, What::Clock),
        (9, What::Env),
        (9, What::Random),
        (10, What::Io),
        (10, What::Io),
        (10, What::Io),
        (10, What::Io),
        (10, What::Io),
    ];
    let got = shipped(&found);
    expect(got == want, &format!("the gate's sites:\n  found  {got:?}\n  wanted {want:?}"));
    // The allowed statics: the table's rows read from its tokens (a comment naming a row is
    // none), a row's static in its crate is no site and is declared, a fourth is a site, and a
    // `thread_local!` is no site only when every static it declares is a row.
    let table = "// AllowedStatic { krate: \"x\", name: \"NOT\", why: Allowed::Allocator },\n\
                 pub const ALLOWED_STATICS: [AllowedStatic; 2] = [\n\
                 AllowedStatic { krate: \"blaze-core\", name: \"HELD\", why: Allowed::SignalHandler },\n\
                 AllowedStatic { krate: \"blaze-mem\", name: \"LOCAL\", why: Allowed::Allocator },\n];\n";
    let allowed = parse_allowed(table).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let rows: Vec<(&str, &str)> = allowed.iter().flat_map(|(k, names)| names.iter().map(move |n| (k.as_str(), n.as_str()))).collect();
    expect(rows == vec![("blaze-core", "HELD"), ("blaze-mem", "LOCAL")], &format!("the allowed table's rows: {rows:?}"));
    expect(
        parse_allowed("AllowedStatic { krate: \"a\", name: \"N\" }\nAllowedStatic { krate: \"a\", name: \"N\" }")
            == Err(SansError::TwoAllowedRows { line: 2, krate: "a".to_owned(), name: "N".to_owned() }),
        "a second row of one static is refused",
    );
    expect(parse_allowed("AllowedStatic { krate: \"a\" }") == Err(SansError::NotAnAllowedRow { line: 1 }), "a row with no name is refused");
    let core_rows = allowed.get("blaze-core").cloned().unwrap_or_default();
    let mem_rows = allowed.get("blaze-mem").cloned().unwrap_or_default();
    let held = scan_tokens_in("static HELD: AtomicBool = AtomicBool::new(false);\nstatic MORE: AtomicBool = AtomicBool::new(false);\n", false, false, &core_rows);
    expect(shipped(&held) == vec![(2, What::Static)] && held.declared == vec!["HELD".to_owned()], &format!("allowed statics: {:?}", held));
    let elsewhere = scan_tokens("static HELD: AtomicBool = AtomicBool::new(false);\n", false, false);
    expect(shipped(&elsewhere) == vec![(1, What::Static)], "a row's name in another crate is a site");
    let local = "std::thread_local! {\n    static LOCAL: Local = const { Local::new() };\n}\n";
    expect(shipped(&scan_tokens_in(local, false, false, &mem_rows)) == vec![(1, What::Std)], "an allowed thread-local is no static site");
    let mixed = "thread_local! {\n    static LOCAL: Cell<u8> = Cell::new(0);\n    static MORE: Cell<u8> = Cell::new(0);\n}\n";
    expect(
        shipped(&scan_tokens_in(mixed, false, false, &mem_rows)) == vec![(1, What::Static), (3, What::Static)],
        "a thread-local with a fourth static is a site, and so is the fourth",
    );
    expect(
        shipped(&scan_tokens("#[global_allocator]\nstatic HEAP: Counted<AtomicU64> = Counted::new();\n", false, false)) == vec![(2, What::Static)],
        "the allocator's attribute alone allows nothing: the table decides",
    );
    // D173's `bare`: a reached crate is bare when its root is `#![no_std]`, it names no
    // `alloc`, no `extern crate std` outside tests, no third-party package, and every crate it
    // names is bare; an exempt crate is judged not and fails nothing that names it.
    let facts = |no_std: bool, alloc: bool, extern_std: bool| {
        let mut it = CrateFacts { root: Some(("crates/x/src/lib.rs".to_owned(), marks_of(if no_std { "#![no_std]\n" } else { "" }))), ..CrateFacts::default() };
        if alloc {
            it.sites.push(SansSite { path: "crates/x/src/lib.rs".to_owned(), line: 3, col: Col::Alloc });
        }
        if extern_std {
            it.extern_std.push("crates/x/src/lib.rs:2".to_owned());
        }
        it
    };
    let mut counts = SansCounts::new();
    for (krate, no_std, alloc, extern_std) in [
        ("leaf", true, false, false),
        ("allocs", true, true, false),
        ("top", true, false, false),
        ("hosted", false, false, false),
        ("std-linked", true, false, true),
        ("third", true, false, false),
        ("uses-wasi", true, false, false),
        ("blaze-wasi", false, true, false),
    ] {
        counts.insert(krate.to_owned(), facts(no_std, alloc, extern_std));
    }
    let reached = parse_reached(
        "# header\ncrates/top\t0\tcrates/leaf,crates/allocs\ncrates/leaf\t0\t-\ncrates/allocs\t0\t-\ncrates/hosted\t0\t-\n\
         crates/std-linked\t0\t-\ncrates/third\t2\t-\ncrates/blaze-wasi\t5\t-\ncrates/uses-wasi\t0\tcrates/blaze-wasi\n",
    )
    .unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let verdicts = bare_crates(&counts, &reached);
    let want: BTreeMap<String, Result<(), NotBare>> = [
        ("allocs", Err(NotBare::Alloc { first: "crates/x/src/lib.rs:3".to_owned() })),
        ("hosted", Err(NotBare::NotNoStd)),
        ("leaf", Ok(())),
        ("std-linked", Err(NotBare::ExternStd { at: "crates/x/src/lib.rs:2".to_owned() })),
        ("third", Err(NotBare::ThirdParty { count: 2 })),
        ("top", Err(NotBare::Dep { krate: "allocs".to_owned() })),
        ("uses-wasi", Ok(())),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    expect(verdicts == want, &format!("the bare verdicts:\n  found  {verdicts:?}\n  wanted {want:?}"));
    add_bare(&mut counts, &verdicts);
    expect(counts.get("top").is_some_and(|it| it.row()[Col::Bare.at()] == 1), "a crate that is not bare is 1 in `bare`");
    expect(counts.get("blaze-wasi").is_some_and(|it| it.row()[Col::Bare.at()] == 0), "an exempt crate is 0 in `bare`");
    expect(
        bare_summary(&counts, &verdicts).starts_with("7 crates reached") && bare_summary(&counts, &verdicts).contains("6 no_std, 2 no_std without alloc"),
        &bare_summary(&counts, &verdicts),
    );
    expect(parse_reached("crates/a\tx\t-\n").is_err(), "a reached row whose count is no number is refused");
    // A bare `fs::` head is the crate's module in a file that declares or imports it, and
    // std's in one that imports it from std or names none (blaze-lint's `OwnHeads`).
    for (text, io) in [
        ("mod fs;\nfn f() { fs::step(x); }\n", 0),
        ("use crate::machine::{fs, other};\nfn f() { fs::step(x); }\n", 0),
        ("use std::fs;\nfn f() { fs::read(p); }\n", 2),
        ("fn f() { fs::read(p); }\n", 1),
    ] {
        let sites = shipped(&scan_tokens(text, false, false));
        let got = sites.iter().filter(|it| it.1 == What::Io).count();
        expect(got == io, &format!("{text:?}: {got} IO sites, wanted {io}"));
    }
    // Test code by its attribute and item, by tokens: the item under `#[cfg(test)]` and a
    // `#[test]` fn are the tests'; the item after is not; `Global` in a test is its own column.
    let tests = "fn a() { let v = Vec::new(); }\n#[cfg(test)]\nmod tests {\n    fn b() { let g = Global; let v = Vec::new(); }\n}\n\
                 #[test]\nfn c() { Alloc::global(); }\n#[cfg(all(test, feature = \"x\"))]\nconst X: [u8; 2] = [0; 2]; fn d() { Box::new(1); }\n";
    let found = scan_tokens(tests, false, false);
    let test_sites: Vec<(usize, What)> = found.sites.iter().filter(|it| it.2).map(|it| (it.0, it.1)).collect();
    expect(shipped(&found) == vec![(1, What::Implicit), (9, What::Implicit)], &format!("code outside tests: {:?}", shipped(&found)));
    expect(
        test_sites == vec![(4, What::Global), (4, What::Implicit), (7, What::AllocGlobal)],
        &format!("test code: {test_sites:?}"),
    );
    // A whole file under `#![cfg(test)]`, and a root's marks.
    let whole = scan_tokens("#![cfg(test)]\nfn f() { Vec::new(); }\n", false, false);
    expect(whole.sites.iter().all(|it| it.2) && whole.marks.test, "a `#![cfg(test)]` file is test code");
    let marks = marks_of("//! a core\n#![no_std]\n#![blaze::sans_io]\n#![deny(blaze::std_use, blaze::sans_io_core)]\n#![forbid(unsafe_code)]\n");
    expect(
        marks.no_std
            && marks.sans_io
            && !marks.driver
            && marks.denied == BTreeSet::from(["std_use".to_owned(), "sans_io_core".to_owned()]),
        &format!("a core's root: {marks:?}"),
    );
    let driver = marks_of("#![cfg_attr(not(test), no_std)]\n#![blaze::driver]\n");
    expect(driver.no_std && driver.driver && !driver.sans_io, &format!("a driver's root: {driver:?}"));
    // Sans-IO code: an item's `#[blaze::sans_io]` and a module's inner one mark their tokens;
    // a blocking call and a clock read there break it; the same outside does not.
    let core = "#[blaze::sans_io]\nmod m {\n    fn f(r: R) { r.recv(); Instant::now(); }\n}\nmod n {\n    #![blaze::sans_io]\n    fn g(x: X) { x.lock(); }\n}\n\
                fn h(r: R) { r.recv(); }\n";
    let found = scan_tokens(core, false, false);
    let breaks: Vec<(usize, What)> = found.sites.iter().filter(|it| it.3 && it.1.breaks()).map(|it| (it.0, it.1)).collect();
    expect(
        breaks == vec![(3, What::Blocking), (3, What::Clock), (7, What::Blocking)],
        &format!("sans-IO code's breaks: {breaks:?}"),
    );
    // `Os::process()` in a `fn main` is the entry's; elsewhere it is IO.
    let entry = scan_tokens("fn helper() { Os::process(); }\nfn main() {\n    let os = Os::process();\n}\n", false, false);
    expect(shipped(&entry) == vec![(1, What::Io)], &format!("the entry's world: {:?}", shipped(&entry)));
    // D179's locks, as blaze-lint's `lock-without-reason` reads them: each constructor is a
    // `locks_constructed` site, and one with no `lock_exception` over it a
    // `locks_without_reason` site too; the reason reaches its statement, field or item.
    let locks = scan_tokens(
        "fn f() {\n\
         \x20   let a = Lock::new(\"a\", 0);\n\
         \x20   #[blaze::lock_exception(tried = \"published\", why = \"every worker writes it\")]\n\
         \x20   let b = bstd::sync::RwLock::<u8>::new(0);\n\
         \x20   let c = SharedMap::new_in(alloc);\n\
         \x20   let d = Lock::default(); let e = Outer::Lock::new(0); let g = x.lock();\n\
         }\n\
         #[blaze::lock_exception(tried = \"claimed\", why = \"one owner\")]\n\
         fn h() -> S {\n\
         \x20   S { a: ReadLock::unrecorded(0), b: RankedLock::new(r, 0) }\n\
         }\n\
         use blaze_core::lock::Lock::new;\n\
         #[test]\n\
         fn t() { let l = Lock::probed(p, 0); }\n",
        false,
        false,
    );
    let lock_sites: Vec<(usize, What)> = shipped(&locks).into_iter().filter(|it| matches!(it.1, What::Lock | What::LockNoReason)).collect();
    expect(
        lock_sites
            == vec![(2, What::Lock), (2, What::LockNoReason), (4, What::Lock), (5, What::Lock), (5, What::LockNoReason), (10, What::Lock), (10, What::Lock)],
        &format!("the lock sites: {lock_sites:?}"),
    );
    // A struct literal's field: the reason covers that field alone; an `if`'s `else` is the
    // same statement.
    let fields = scan_tokens(
        "fn f() -> S {\n\
         \x20   S {\n\
         \x20       #[blaze::lock_exception(tried = \"gathered\", why = \"x\")]\n\
         \x20       a: Lock::new(\"a\", 0),\n\
         \x20       b: Lock::new(\"b\", 0),\n\
         \x20   }\n\
         }\n\
         fn g() {\n\
         \x20   #[blaze::lock_exception(tried = \"gathered\", why = \"x\")]\n\
         \x20   if c { x = Lock::new(\"c\", 0); } else { x = Lock::new(\"d\", 0); }\n\
         \x20   y = Lock::new(\"e\", 0);\n\
         }\n",
        false,
        false,
    );
    let unreasoned: Vec<usize> = shipped(&fields).into_iter().filter(|it| it.1 == What::LockNoReason).map(|it| it.0).collect();
    expect(unreasoned == vec![5, 11], &format!("the locks a field's and a branch's reason leave: {unreasoned:?}"));
    // A done crate may hold a lock with its reason, not one without.
    let lock_site = |line, col| SansSite { path: "crates/l/src/lib.rs".to_owned(), line, col };
    let marks = Marks { no_std: true, sans_io: true, denied: ["std_use", "implicit_allocator", "process_static", "sans_io_core"].map(str::to_owned).into(), ..Marks::default() };
    let reasoned = CrateFacts {
        root: Some(("crates/l/src/lib.rs".to_owned(), marks.clone())),
        package: Some("l".to_owned()),
        sites: vec![lock_site(3, Col::Locks)],
        declared: BTreeMap::new(),
        extern_std: Vec::new(),
        exported: BTreeSet::new(),
        macro_paths: Vec::new(),
    };
    let done = [(1, "l".to_owned())];
    let mut lock_row = [0; WIDTH];
    lock_row[Col::Locks.at()] = 1;
    let (over, _) = sans_problems(&BTreeMap::from([("l".to_owned(), reasoned)]), &BTreeMap::from([("l".to_owned(), lock_row)]), None, None, &done, &BTreeMap::new());
    expect(over.is_empty(), &format!("a done crate with a reasoned lock: {over:?}"));
    let bare = CrateFacts {
        root: Some(("crates/l/src/lib.rs".to_owned(), marks)),
        package: Some("l".to_owned()),
        sites: vec![lock_site(3, Col::Locks), lock_site(3, Col::LocksNoReason)],
        declared: BTreeMap::new(),
        extern_std: Vec::new(),
        exported: BTreeSet::new(),
        macro_paths: Vec::new(),
    };
    lock_row[Col::LocksNoReason.at()] = 1;
    let (over, _) = sans_problems(&BTreeMap::from([("l".to_owned(), bare)]), &BTreeMap::from([("l".to_owned(), lock_row)]), None, None, &done, &BTreeMap::new());
    expect(
        over.len() == 1 && over[0].contains("`locks_without_reason`") && over[0].contains(DONE_LIST),
        &format!("a done crate with a lock without its reason: {over:?}"),
    );
    // A tool attribute at a crate root and the crate's own exported macro named by path
    // (agents/annoyances/crate-root-tool-attribute-hides-macros.json): the three shapes of
    // 2026-11-20, each refused by name, and what is not one.
    let crate_of_files = |files: &[(&str, &str)]| {
        let mut facts = CrateFacts::default();
        for (path, text) in files {
            let found = scan_tokens(text, false, false);
            if facts.root.is_none() {
                facts.root = Some(((*path).to_owned(), found.marks.clone()));
            }
            facts.add_macros(path, found.exported, found.macro_paths);
        }
        facts
    };
    let refused = |name: &str, files: &[(&str, &str)]| macro_path_problems(&BTreeMap::from([(name.to_owned(), crate_of_files(files))]));
    // blaze-phf: `#![blaze::sans_io]`, and the crate calls its exported macro by `crate::`.
    let phf = refused("blaze-phf", &[
        ("crates/blaze-phf/src/lib.rs", "#![no_std]\n#![blaze::sans_io]\nmod table;\n#[macro_export]\nmacro_rules! name_phf_table {\n    ($($t:tt)*) => {};\n}\n"),
        ("crates/blaze-phf/src/table.rs", "crate::name_phf_table!(pub static T = WORDS;);\n"),
    ]);
    expect(
        phf.len() == 1
            && phf[0].starts_with("`blaze-phf`: its root crates/blaze-phf/src/lib.rs says `#![blaze::sans_io]`")
            && phf[0].contains("crates/blaze-phf/src/table.rs:1 `name_phf_table!`")
            && phf[0].contains("bare name")
            && phf[0].contains("integration test"),
        &format!("a `crate::` call of an exported macro under a root tool attribute: {phf:?}"),
    );
    // blaze-sim: the exported macro names itself, or another of the crate's, by `$crate::`.
    let sim = refused("blaze-sim", &[(
        "crates/blaze-sim/src/lib.rs",
        "#![blaze::sans_io]\n#[macro_export]\n#[doc(hidden)]\nmacro_rules! check {\n    ($x:expr) => { $crate::check_with!($x, 1) };\n}\n#[macro_export]\nmacro_rules! check_with {\n    ($x:expr, $n:expr) => {};\n}\n",
    )]);
    expect(sim.len() == 1 && sim[0].contains("lib.rs:5 `check_with!`"), &format!("a `$crate::` path in an exported macro's body: {sim:?}"));
    // blaze-rng: `#![blaze::trust(..)]`, and a unit test in the library names the macro.
    let rng = refused("blaze-rng", &[
        ("crates/blaze-rng/src/lib.rs", "#![blaze::trust(rng)]\n#[macro_export]\nmacro_rules! seeded { () => {}; }\n#[cfg(test)]\nmod tests;\n"),
        ("crates/blaze-rng/src/tests.rs", "#[test]\nfn t() { crate::seeded!(); }\n"),
    ]);
    expect(rng.len() == 1 && rng[0].contains("`#![blaze::trust]`") && rng[0].contains("src/tests.rs:2 `seeded!`"), &format!("a unit test's `crate::` path: {rng:?}"));
    // Not one: a bare-name call, no tool attribute at the root, the caller in an integration
    // test or a binary, a macro that is not exported, a longer path, a tool attribute below
    // the root.
    let lib = "#![blaze::sans_io]\n#[macro_export]\nmacro_rules! m { () => {}; }\n";
    for files in [
        vec![("crates/x/src/lib.rs", "#![blaze::sans_io]\n#[macro_export]\nmacro_rules! m { () => {}; }\nfn f() { m!(); }\n")],
        vec![("crates/x/src/lib.rs", "#![no_std]\n#[macro_export]\nmacro_rules! m { () => {}; }\nfn f() { crate::m!(); }\n")],
        vec![("crates/x/src/lib.rs", lib), ("crates/x/tests/t.rs", "fn t() { crate::m!(); }\n")],
        vec![("crates/x/src/lib.rs", lib), ("crates/x/src/bin/x.rs", "fn main() { crate::m!(); }\n")],
        vec![("crates/x/src/lib.rs", "#![blaze::sans_io]\nmacro_rules! m { () => {}; }\nfn f() { crate::m!(); }\n")],
        vec![("crates/x/src/lib.rs", lib), ("crates/x/src/a.rs", "fn f() { crate::a::m!(); }\n")],
        vec![("crates/x/src/lib.rs", "#![no_std]\nmod a;\n#[macro_export]\nmacro_rules! m { () => {}; }\n"), ("crates/x/src/a.rs", "#![blaze::sans_io]\nfn f() { crate::m!(); }\n")],
    ] {
        let found = refused("x", &files);
        expect(found.is_empty(), &format!("refused what is no root tool attribute's macro path: {files:?} -> {found:?}"));
    }
    // The lists: a crate over its row is refused naming the column and its sites; under is a
    // note; a done crate with a site, a third-party row or a lint not denied is refused.
    let site = |line, col| SansSite { path: "crates/a/src/lib.rs".to_owned(), line, col };
    let facts = CrateFacts {
        root: Some(("crates/a/src/lib.rs".to_owned(), Marks { no_std: true, sans_io: true, ..Marks::default() })),
        package: Some("a".to_owned()),
        sites: vec![site(3, Col::Implicit), site(9, Col::Implicit)],
        declared: BTreeMap::new(),
        extern_std: Vec::new(),
        exported: BTreeSet::new(),
        macro_paths: Vec::new(),
    };
    let counts: SansCounts = BTreeMap::from([("a".to_owned(), facts)]);
    let mut held = [0; WIDTH];
    held[Col::Implicit.at()] = 1;
    let rows = BTreeMap::from([("a".to_owned(), held)]);
    let added = BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([9]))]);
    let (over, under) = sans_problems(&counts, &rows, None, Some(&added), &[], &BTreeMap::new());
    expect(
        over.len() == 1 && over[0].contains("crates/a/src/lib.rs:9, crates/a/src/lib.rs:3") && over[0].contains("`implicit`") && under.is_empty(),
        &format!("one over its row, the new site first: {over:?}"),
    );
    let mut high = held;
    high[Col::Implicit.at()] = 5;
    let (over, under) = sans_problems(&counts, &BTreeMap::from([("a".to_owned(), high)]), None, None, &[], &BTreeMap::new());
    expect(over.is_empty() && under.len() == 1, &format!("under its row is a note: {over:?} {under:?}"));
    let (over, _) = sans_problems(&counts, &BTreeMap::from([("a".to_owned(), high)]), Some(&rows), None, &[], &BTreeMap::new());
    expect(over.len() == 1 && over[0].contains("was 1 at the base"), &format!("a row raised since the base: {over:?}"));
    let done = vec![(2, "a".to_owned()), (3, "b".to_owned())];
    let tp = BTreeMap::from([("a".to_owned(), [1, 0, 4])]);
    let (over, _) = sans_problems(&counts, &BTreeMap::from([("a".to_owned(), high)]), None, None, &done, &tp);
    expect(
        over.len() == 4
            && over.iter().any(|it| it.contains("is in tools/ratchets/sans-io-done.txt but has 2"))
            && over.iter().any(|it| it.contains("third-party"))
            && over.iter().any(|it| it.contains("`blaze::sans_io_core`"))
            && over.iter().any(|it| it.contains("`b` is no crate")),
        &format!("a done crate is held at 0: {over:?}"),
    );
    // The list reads back, and a write never raises a row.
    let rendered = render_sans(&counts);
    let mut two = [0; WIDTH];
    two[Col::Implicit.at()] = 2;
    expect(parse_sans_rows(&rendered) == Ok(BTreeMap::from([("a".to_owned(), two)])), &format!("the list reads back: {rendered}"));
    expect(held_sans(&counts, &rows) == rows, "a write keeps the lower");
    expect(held_sans(&counts, &BTreeMap::new()).is_empty(), "a crate with no row stays without one");
    expect(
        parse_sans_rows("a\t1\n").is_err() && parse_numbers::<3>("a\t1\t0\t0\na\t1\t0\t0\n", THIRD_PARTY_LIST).is_err(),
        "a bad row is refused",
    );
    expect(package_name("[package]\nname = \"blaze-x\" # it\n[lib]\nname = \"y\"\n").as_deref() == Some("blaze-x"), "a package's name");
    expect(parse_done("# header\n\nblaze-h1\n") == vec![(3, "blaze-h1".to_owned())], "the done list");
}
