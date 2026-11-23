//! The one pass: an item's own bytes lexed once, each significant token dispatched to the rules
//! registered for its kind, a block stack and the test attributes tracked on the way, and the
//! candidates filtered by test code and written out at the end.
//!
//! Every rule here is a port of its script under `scripts/src/bin/` with the same matches and
//! exemptions, read over tokens instead of bytes; where the two read differently, the comment at
//! the rule says how (docs/v2/plan/design/lint.md lists them all).

use blaze_lex::{Cpu, FrontmatterAllowed, LiteralKind, TokenKind, tokenize};
use blaze_memchr::{memchr, memrchr};
use blaze_own::{AVec, Alloc};

use crate::matcher::{DEAD, Live, NO_SEQ};
use crate::sequences::{
    ArgOf, BANG, BAR, BRACE, CLOSE, Cut, DOT, DOTDOT, Guard, LT, MAX_WORD_LEN, MIN_WORD_LEN, NE, NO_WORD, OPEN, OTHER,
    SEP, SEQS, TRIE, W_ALLOC, W_ALLOC_TYPE, W_ARC, W_AS, W_BLOCK_ON, W_CLONE, W_CRATE, W_EXPECT, W_EXTERN, W_FALSE,
    W_FILE, W_FIND, W_FN, W_FOR, W_GET, W_GLOBAL, W_GLOBAL_FN, W_IMPL, W_IN, W_INSTANT, W_IS_SOME_AND, W_JOIN, W_LEN,
    W_LET, W_LIBC, W_LOCK, W_MOD, W_MUT, W_ORDERING, W_RECV, W_REF, W_RUSTIX, W_SELF, W_SEQCST, W_SOME, W_STARTS_WITH, W_STD,
    W_STRING, W_SYSTEM_TIME, W_THREAD_LOCAL, W_TRAIT, W_TRUE, W_TRY_INTO, W_TYPE, W_UNIQ, W_UNSAFE, W_UNWRAP, W_USE, W_VEC,
    W_WAIT, WC_COLLECTION, WC_DEFINING, WC_DISPATCHED, WC_IO_MODULE, WC_IO_TYPE, WC_LOCK_TYPE, WC_PRINT_MACRO,
    WC_RENAME, WC_SANS_IO, WC_STATIC, WORD, WORD_CLASS, WORD_FIRST_BY_LEN,
};
use crate::{
    Arg, Block, Cost, Finding, Findings, Fix, LintError, Mark, ONLY_OWN, Owner, Rule, RuleSet, Run, Scope, Span,
    Template, TestBits, Unit, Walk, count,
};

/// The rules that have sequences in `sequences::SEQS`: when none of them runs, the matcher's
/// pass is not made.
const SEQUENCE_RULES: [Rule; 11] = [
    Rule::Stops,
    Rule::FormatMacro,
    Rule::AmbientHeap,
    Rule::NoAsync,
    Rule::AmbientRead,
    Rule::LeftUndone,
    Rule::UnboundedWait,
    Rule::AmbientHasher,
    Rule::RawSync,
    Rule::BlockingOnPool,
    Rule::Transmute,
];

/// A token's kind, as the rules tell kinds apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum K {
    Ident,
    /// `r#name` or `k#name`: never a keyword.
    Raw,
    Life,
    Int,
    Float,
    /// Any other literal: strings, characters, byte strings.
    Lit,
    /// A one-character punctuation token.
    P(u8),
    /// A child item's range, left out of this item's own bytes.
    Hole,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tok {
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) k: K,
    /// An identifier's number in the rules' word table (`sequences::WORDS`), found once while
    /// lexing; `NO_WORD` for every other token and for a word no rule reads.
    word: u8,
}

/// A `{` open, and what opened it.
#[derive(Clone, Copy)]
struct Frame {
    block: Block,
}

/// A function seen by the recursion rule.
#[derive(Clone, Copy)]
struct FnRec {
    /// Its `fn` token's index: where its bindings are looked for from.
    at: u32,
    name: Span,
    assoc: bool,
    has_self: bool,
    recursive: bool,
    /// `// recursion: calls another impl` within three lines above its `fn`.
    marked: bool,
}

/// A signature being read (recursion.rs's): its function and its `(`/`[` depth.
#[derive(Clone, Copy)]
struct Sig {
    item: u32,
    depth: u32,
}

/// A fix a candidate carries, written out with it.
#[derive(Clone, Copy)]
pub(crate) enum Plan {
    None,
    /// The method name at `name` becomes row `row`'s new name, and when the row has a suffix,
    /// it goes at `after` (the call's `)` end).
    Rename { row: u32, name: Span, after: u32 },
    /// These four tokens deleted, each a whole token (`dbg`, `!`, `(`, `)` around a value).
    Strip { tokens: [Span; 4] },
    /// Replace one token span with fixed text (e.g. `mut` -> `uniq` in `&mut` / `ref mut`).
    Replace { range: Span, text: &'static str },
    /// Rewrite `&recv[..N] == RHS` -> `recv.starts_with(RHS)` in three disjoint edits:
    /// delete `&`, replace `[..N] == ` with `.starts_with(`, and insert `)` after `RHS`.
    WrapStartsWith { amp: Span, middle: Span, after_rhs: u32 },
    /// Rewrite `expr as T` -> `T::from(expr)` (or `(expr) as T` -> `T::from(expr)` without double
    /// parens when `parens` is false).
    WrapFrom { before_expr: u32, after_expr: u32, as_end: u32, prefix: &'static str, add_close: bool },
    /// Insert `!` at `bang_at` and replace `range` with `text` (e.g. `x.len() > 0` -> `!x.is_empty()`).
    NegateReplace { bang_at: u32, range: Span, text: &'static str },
    /// `range` replaced by the UTF-8 slice `text` in `Pass::fix_buf` (empty for deletion).
    ReplaceBuf { range: Span, text: Run },
}

/// A finding before test code is known.
#[derive(Clone, Copy)]
pub(crate) struct Cand {
    pub(crate) template: Template,
    pub(crate) range: Span,
    pub(crate) arg: Option<Arg>,
    pub(crate) plan: Plan,
}

/// How many lines above a clone or an `unsafe` its comment is looked for (clones.rs's `LOOK_UP`).
const LOOK_UP: u8 = 8;
/// How many lines above its `fn` a recursion's mark may be (recursion.rs's `LOOK_UP`).
const RECURSION_LOOK_UP: u8 = 3;
const CLONE_MARK: &str = "// clone:";
const RECURSION_MARK: &str = "// recursion: calls another impl";

const INTEGERS: [&str; 12] = ["u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize"];
const ALLOCATING: [(&str, usize); 9] = [
    ("Vec", 2),
    ("VecDeque", 2),
    ("Box", 2),
    ("Rc", 2),
    ("Arc", 2),
    ("BTreeMap", 3),
    ("BTreeSet", 2),
    ("BinaryHeap", 2),
    ("LinkedList", 2),
];
#[derive(Clone, Copy, PartialEq, Eq)]
enum HandleKind {
    /// Thread-local non-atomic handle (`Rc`, `ARc`): autofix to `.local_share()`.
    Local,
    /// Cross-thread atomic handle (`Arc`, `AArc`, `ThinArc`, `AThinArc`, `ThinStr`, `AThinStr`): autofix to `.share()`.
    Shared,
}

fn classify_handle(word: &str) -> Option<HandleKind> {
    match word {
        "Rc" | "ARc" => Some(HandleKind::Local),
        "Arc" | "AArc" | "ThinArc" | "AThinArc" | "ThinStr" | "AThinStr" => Some(HandleKind::Shared),
        _ => None,
    }
}
const HANDLE_WORDS: [&str; 11] =
    ["refcount", "refcounts", "count", "counts", "handle", "handles", "arc", "rc", "interned", "cursor", "cursors"];
const HANDLE_PHRASES: [&str; 2] = ["allocates nothing", "no allocation"];
const ERROR_WORDS: [&str; 4] = ["error", "errors", "diagnostic", "diagnostics"];
const ERROR_PHRASES: [&str; 1] = ["error path"];
const DEEP_WORDS: [&str; 19] = [
    "vec", "vecs", "vector", "vectors", "string", "strings", "map", "maps", "set", "sets", "list", "lists", "box", "copy",
    "copies", "deep", "allocates", "allocation", "allocations",
];
const NOT_BEFORE_INDEX: [&str; 22] = [
    "in", "let", "return", "break", "mut", "for", "where", "impl", "dyn", "as", "else", "match", "if", "while", "move", "yield",
    "unsafe", "const", "static", "ref", "box", "type",
];
pub(crate) const COLLECTIONS: [&str; 11] =
    ["Vec", "Box", "String", "Rc", "Arc", "BTreeMap", "BTreeSet", "VecDeque", "BinaryHeap", "HashMap", "HashSet"];
pub(crate) const DEFINING: [&str; 5] = ["struct", "enum", "union", "trait", "type"];
/// The formatting macros `format-macro` finds (its sequences are in `sequences::SEQS`).
pub(crate) const FORMAT_MACROS: [&str; 8] =
    ["format", "format_args", "write", "writeln", "print", "println", "eprint", "eprintln"];
/// Every word [`Pass::on_word`] matches on by name: each must be in `sequences::WORDS`, which a
/// compile-time check there holds, since a word not in the table is never dispatched.
pub(crate) const DISPATCHED: [&str; 25] = [
    "use", "as", "clone", "Global", "fn", "type", "impl", "trait", "mod", "for", "self", "unwrap", "expect", "std",
    "alloc", "unsafe", "Ordering", "ref", "let", "get", "starts_with", "len", "find", "Alloc", "extern",
];
/// The words `process-static` starts at ([`Pass::process_static`]).
pub(crate) const STATIC_WORDS: [&str; 2] = ["static", "thread_local"];
/// The words `sans-io-core` starts at in a core ([`Pass::sans_io_word`]): the IO modules, the
/// IO types and crates, the blocking calls, the clock types, the print macros.
pub(crate) const SANS_IO_WORDS: [&str; 25] = [
    "fs", "net", "process", "io", "os", "thread", "env", "File", "TcpStream", "TcpListener", "UdpSocket", "libc", "rustix",
    "recv", "join", "wait", "lock", "block_on", "Instant", "SystemTime", "print", "println", "eprint", "eprintln", "dbg",
];
/// The modules of `std` a core never names: a path through one is IO, a thread or the process.
/// Their order is [`crate::OwnHeads`]'s bits.
pub(crate) const IO_MODULES: [&str; 7] = ["fs", "net", "process", "io", "os", "thread", "env"];
/// The IO types a core never names, and the system-call crates.
pub(crate) const IO_TYPES: [&str; 5] = ["TcpStream", "TcpListener", "UdpSocket", "libc", "rustix"];
/// What a `File::` call does that is IO.
const FILE_CALLS: [&str; 4] = ["open", "create", "create_new", "options"];
pub(crate) const PRINT_MACROS: [&str; 5] = ["print", "println", "eprint", "eprintln", "dbg"];
/// The lock types `lock-without-reason` starts at ([`Pass::lock_ctor`]): blaze-core's lock.rs
/// (`Lock`, `ReadLock`), bstd's (`Lock`, `RwLock`, `RankedLock`) and `SharedMap`.
pub(crate) const LOCK_TYPES: [&str; 5] = ["Lock", "ReadLock", "RwLock", "RankedLock", "SharedMap"];
/// The values of a `lock_exception`'s `tried`: D179's three shapes, as written.
const LOCK_SHAPES: [&str; 3] = ["\"published\"", "\"gathered\"", "\"claimed\""];

/// One test-code region, augmented with prefix-max `max_end` so [`Pass::test_at`] binary-searches
/// and stops as soon as no earlier region can reach `pos`.
#[derive(Clone, Copy)]
pub(crate) struct TestRegion {
    span: Span,
    flavor: TestBits,
    max_end: u32,
}

/// Reusable per-pass work buffers, kept across units by [`crate::Units::lint_all`] and
/// [`crate::lint_in_with`] / [`crate::lint_own_with`] to eliminate per-unit heap allocations.
pub struct Scratch<'a> {
    pub(crate) alloc: Alloc<'a>,
    toks: AVec<'a, Tok>,
    cands: AVec<'a, Cand>,
    fix_buf: AVec<'a, u8>,
    regions: AVec<'a, TestRegion>,
    reasons: AVec<'a, Span>,
    frames: AVec<'a, Frame>,
    bodies: AVec<'a, u32>,
    fns: AVec<'a, FnRec>,
    pub(crate) children: AVec<'a, Scope>,
}

impl<'a> Scratch<'a> {
    /// Allocates an empty scratch workspace backed by `alloc`.
    pub fn new_in(alloc: Alloc<'a>) -> Scratch<'a> {
        Scratch {
            alloc,
            toks: AVec::new_in(alloc),
            cands: AVec::new_in(alloc),
            fix_buf: AVec::new_in(alloc),
            regions: AVec::new_in(alloc),
            reasons: AVec::new_in(alloc),
            frames: AVec::new_in(alloc),
            bodies: AVec::new_in(alloc),
            fns: AVec::new_in(alloc),
            children: AVec::new_in(alloc),
        }
    }

    /// Empties every work buffer while retaining allocated capacities for the next unit.
    pub fn clear(&mut self) {
        self.toks.clear();
        self.cands.clear();
        self.fix_buf.clear();
        self.regions.clear();
        self.reasons.clear();
        self.frames.clear();
        self.bodies.clear();
        self.fns.clear();
        self.children.clear();
    }

    /// The child scopes produced by the most recent pass run with this scratch.
    #[inline]
    pub fn children(&self) -> &[Scope] {
        &self.children
    }
}

/// Non-allocating summary returned by [`run_with`]; child scopes live in `scratch.children()`.
#[derive(Clone, Copy)]
pub(crate) struct PassDone {
    pub(crate) findings: Run,
    pub(crate) test_at_end: TestBits,
    pub(crate) cost: Cost,
    pub(crate) tokens: u32,
}

pub(crate) fn run_with<'a>(
    rules: &RuleSet,
    unit: &Unit<'_>,
    scratch: &mut Scratch<'a>,
    out: &mut Findings<'a>,
) -> Result<PassDone, LintError> {
    if u32::try_from(unit.text.len()).is_err() {
        return Err(LintError::TooLong { len: unit.text.len() });
    }
    scratch.clear();
    // Sized by the unit's own bytes (about one token per five), not its children's; only grows
    // when a unit exceeds the high-water mark already held by `scratch`.
    let need_toks = own_bytes(unit) / 5 + 8;
    if scratch.toks.capacity() < need_toks {
        scratch.toks.reserve(need_toks);
    }
    if scratch.children.capacity() < unit.holes.len() {
        scratch.children.reserve(unit.holes.len());
    }
    let mut pass = Pass {
        rules,
        text: unit.text,
        bytes: unit.text.as_bytes(),
        scope: unit.scope,
        toks: &mut scratch.toks,
        cands: &mut scratch.cands,
        fix_buf: &mut scratch.fix_buf,
        regions: &mut scratch.regions,
        reasons: &mut scratch.reasons,
        frames: &mut scratch.frames,
        bodies: &mut scratch.bodies,
        fns: &mut scratch.fns,
        children: &mut scratch.children,
        header: None,
        sig: None,
        in_use: false,
        defining: false,
        last_fn: None,
        cost: Cost::NONE,
    };
    pass.lex(unit.holes)?;
    pass.walk();
    if SEQUENCE_RULES.iter().any(|rule| rules.runs(*rule)) {
        pass.sequences();
    }
    if rules.runs(Rule::IoRef) {
        pass.io_ref_pass();
    }
    if rules.runs(Rule::UnusedImport) {
        pass.unused_import_pass(false);
    }
    if rules.runs(Rule::UnusedMut) {
        pass.unused_mut_pass();
    }
    if rules.runs(Rule::HeapLeak) {
        pass.heap_leak_pass();
    }
    let test_at_end = pass.test_at(unit.text.len().saturating_sub(1) as u32);
    let from = count(out.list.len())?;
    pass.emit(out)?;
    let to = count(out.list.len())?;
    // Holes are tokens of the walk too, but not of the lex: the item's own tokens alone.
    let tokens = count(pass.toks.len().saturating_sub(unit.holes.len()))?;
    Ok(PassDone { findings: Run { from, to }, test_at_end, cost: pass.cost, tokens })
}

/// Whole-file top-level `use` pass for [`Rule::UnusedImport`], separate from [`run_with`] so
/// per-item passes remain strictly independent of sibling items.
pub(crate) fn run_file_imports<'a>(
    text: &str,
    scope: Scope,
    scratch: &mut Scratch<'a>,
    out: &mut Findings<'a>,
) -> Result<PassDone, LintError> {
    if u32::try_from(text.len()).is_err() {
        return Err(LintError::TooLong { len: text.len() });
    }
    let only = RuleSet::only(&[Rule::UnusedImport]);
    scratch.clear();
    let mut pass = Pass {
        rules: &only,
        text,
        bytes: text.as_bytes(),
        scope,
        toks: &mut scratch.toks,
        cands: &mut scratch.cands,
        fix_buf: &mut scratch.fix_buf,
        regions: &mut scratch.regions,
        reasons: &mut scratch.reasons,
        frames: &mut scratch.frames,
        bodies: &mut scratch.bodies,
        fns: &mut scratch.fns,
        children: &mut scratch.children,
        header: None,
        sig: None,
        in_use: false,
        defining: false,
        last_fn: None,
        cost: Cost::NONE,
    };
    pass.lex(&[])?;
    pass.unused_import_pass(true);
    let from = count(out.list.len())?;
    pass.emit(out)?;
    let to = count(out.list.len())?;
    let tokens = count(pass.toks.len())?;
    Ok(PassDone { findings: Run { from, to }, test_at_end: TestBits::NONE, cost: pass.cost, tokens })
}

/// The bytes of `unit` outside its holes (holes not yet checked: a bad one only mis-sizes).
fn own_bytes(unit: &Unit<'_>) -> usize {
    let holes: usize = unit.holes.iter().map(|h| h.end.saturating_sub(h.start) as usize).sum();
    unit.text.len().saturating_sub(holes)
}

pub(crate) struct Pass<'u, 's, 'a> {
    rules: &'u RuleSet,
    pub(crate) text: &'u str,
    pub(crate) bytes: &'u [u8],
    scope: Scope,
    pub(crate) toks: &'s mut AVec<'a, Tok>,
    pub(crate) cands: &'s mut AVec<'a, Cand>,
    pub(crate) fix_buf: &'s mut AVec<'a, u8>,
    /// Test code: each region's range, flavor, and prefix-max end.
    regions: &'s mut AVec<'a, TestRegion>,
    /// What each `#[blaze::lock_exception(..)]` covers (D179): the statement, field, `static`,
    /// `const` or item after it ([`Pass::reason_end`]).
    reasons: &'s mut AVec<'a, Span>,
    frames: &'s mut AVec<'a, Frame>,
    /// The functions whose bodies are open, innermost last (indices of `fns`).
    bodies: &'s mut AVec<'a, u32>,
    fns: &'s mut AVec<'a, FnRec>,
    children: &'s mut AVec<'a, Scope>,
    /// From `impl`, `trait` or `mod` to the `{` or `;` after it: the block it opens.
    header: Option<Block>,
    sig: Option<Sig>,
    /// From the word `use` to its `;`.
    in_use: bool,
    /// The word before was `struct`, `enum`, `union`, `trait` or `type`.
    defining: bool,
    /// The last `fn` read: where a local's declaration is looked for from.
    last_fn: Option<usize>,
    /// The sites each rule tried ([`Cost`]): one count at each dispatch into a rule.
    pub(crate) cost: Cost,
}

impl<'u, 's, 'a> Pass<'u, 's, 'a> {
    // ---- the lex ------------------------------------------------------------------------

    fn lex(&mut self, holes: &[Span]) -> Result<(), LintError> {
        let len = self.text.len();
        let mut at = 0usize;
        for (index, hole) in holes.iter().enumerate() {
            let (start, end) = (hole.start as usize, hole.end as usize);
            let fits = start >= at
                && end >= start
                && end <= len
                && self.text.is_char_boundary(start)
                && self.text.is_char_boundary(end);
            if !fits {
                return Err(LintError::BadHole { index, hole: *hole });
            }
            self.lex_run(at, start);
            self.toks.push(Tok { start: hole.start, end: hole.end, k: K::Hole, word: NO_WORD });
            at = end;
        }
        self.lex_run(at, len);
        Ok(())
    }

    fn lex_run(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let slice = &self.bytes[from..to];
        if is_ignorable_gap(slice, true) {
            if self.rules.runs(Rule::Allocs) && memchr(b'/', slice).is_some() {
                self.scan_gap_alloc_comments(from, to);
            }
            return;
        }
        let mut pos = from as u32;
        for token in tokenize(&self.text[from..to], FrontmatterAllowed::No, Cpu::BASELINE) {
            let start = pos;
            let end = pos + token.len;
            pos = end;
            let k = match token.kind {
                TokenKind::Whitespace | TokenKind::BlockComment { .. } => continue,
                TokenKind::LineComment { .. } => {
                    if self.rules.runs(Rule::Allocs) {
                        self.cost.add(Rule::Allocs);
                        self.alloc_comment(start, end);
                    }
                    continue;
                }
                TokenKind::Eof => break,
                TokenKind::Ident => K::Ident,
                TokenKind::RawIdent | TokenKind::ForcedKeywordIdent => K::Raw,
                TokenKind::Lifetime { .. } | TokenKind::RawLifetime => K::Life,
                TokenKind::Literal { kind: LiteralKind::Int { .. }, .. } => K::Int,
                TokenKind::Literal { kind: LiteralKind::Float { .. }, .. } => K::Float,
                TokenKind::Literal { .. } => K::Lit,
                TokenKind::Semi => K::P(b';'),
                TokenKind::Comma => K::P(b','),
                TokenKind::Dot => K::P(b'.'),
                TokenKind::OpenParen => K::P(b'('),
                TokenKind::CloseParen => K::P(b')'),
                TokenKind::OpenBrace => K::P(b'{'),
                TokenKind::CloseBrace => K::P(b'}'),
                TokenKind::OpenBracket => K::P(b'['),
                TokenKind::CloseBracket => K::P(b']'),
                TokenKind::At => K::P(b'@'),
                TokenKind::Pound => K::P(b'#'),
                TokenKind::Tilde => K::P(b'~'),
                TokenKind::Question => K::P(b'?'),
                TokenKind::Colon => K::P(b':'),
                TokenKind::Dollar => K::P(b'$'),
                TokenKind::Eq => K::P(b'='),
                TokenKind::Bang => K::P(b'!'),
                TokenKind::Lt => K::P(b'<'),
                TokenKind::Gt => K::P(b'>'),
                TokenKind::Minus => K::P(b'-'),
                TokenKind::And => K::P(b'&'),
                TokenKind::Or => K::P(b'|'),
                TokenKind::Plus => K::P(b'+'),
                TokenKind::Star => K::P(b'*'),
                TokenKind::Slash => K::P(b'/'),
                TokenKind::Caret => K::P(b'^'),
                TokenKind::Percent => K::P(b'%'),
                _ => K::Other,
            };
            // The one probe per identifier: its number in the rules' word table, or none.
            let word = match k {
                K::Ident => {
                    let len = (end - start) as usize;
                    if len >= MIN_WORD_LEN && len <= MAX_WORD_LEN {
                        let first = self.bytes[start as usize].wrapping_sub(b'A');
                        if first < 64 && (WORD_FIRST_BY_LEN[len] & (1u64 << first)) != 0 {
                            match WORD.index(&self.bytes[start as usize..end as usize]) {
                                Some(at) => at as u8,
                                None => NO_WORD,
                            }
                        } else {
                            NO_WORD
                        }
                    } else {
                        NO_WORD
                    }
                }
                _ => NO_WORD,
            };
            self.toks.push(Tok { start, end, k, word });
        }
    }

    /// Runs `alloc_comment` on each `//` line comment inside a gap already verified by
    /// `is_ignorable_gap` to contain only ASCII whitespace and `//` line comments.
    fn scan_gap_alloc_comments(&mut self, from: usize, to: usize) {
        let mut i = from;
        // Bounded: each turn advances `i` past whitespace or one line comment.
        while i < to {
            match self.bytes[i] {
                b' ' | b'\n' | b'\r' | b'\t' => i += 1,
                b'/' => {
                    let end = memchr(b'\n', &self.bytes[i + 2..to]).map_or(to, |nl| i + 2 + nl);
                    self.cost.add(Rule::Allocs);
                    self.alloc_comment(i as u32, end as u32);
                    i = if end < to { end + 1 } else { to };
                }
                _ => break,
            }
        }
    }

    // ---- token access -------------------------------------------------------------------

    #[inline]
    fn tok(&self, i: usize) -> Option<Tok> {
        self.toks.get(i).copied()
    }

    #[inline]
    fn back(&self, i: usize, n: usize) -> Option<Tok> {
        i.checked_sub(n).and_then(|j| self.tok(j))
    }

    #[inline]
    fn str_of(&self, t: Tok) -> &'u str {
        &self.text[t.start as usize..t.end as usize]
    }

    #[inline]
    fn word(&self, t: Option<Tok>) -> Option<&'u str> {
        match t {
            Some(t) if t.k == K::Ident => Some(self.str_of(t)),
            _ => None,
        }
    }

    #[inline]
    fn is_word(&self, t: Option<Tok>, w: &str) -> bool {
        self.word(t) == Some(w)
    }

    /// `::` starting at token `i`.
    #[inline]
    fn colons_at(&self, i: usize) -> bool {
        match (self.tok(i), self.tok(i + 1)) {
            (Some(a), Some(b)) => a.k == K::P(b':') && b.k == K::P(b':') && a.end == b.start,
            _ => false,
        }
    }

    /// `::` just before token `i`.
    #[inline]
    fn colons_before(&self, i: usize) -> bool {
        i >= 2 && self.colons_at(i - 2)
    }

    /// The word at `i` follows a `.` that is not a range's `..`.
    #[inline]
    fn method(&self, i: usize) -> bool {
        match self.back(i, 1) {
            Some(dot) if dot.k == K::P(b'.') => !matches!(self.back(i, 2), Some(t) if t.k == K::P(b'.') && t.end == dot.start),
            _ => false,
        }
    }

    /// The `)`, `]` or `}` that closes the bracket at `open`, holes skipped.
    fn closing(&self, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut j = open;
        // Bounded: each turn moves `j` forward, to the last token at most.
        while let Some(t) = self.tok(j) {
            match t.k {
                K::P(b'(' | b'[' | b'{') => depth += 1,
                K::P(b')' | b']' | b'}') => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
            j += 1;
        }
        None
    }

    // ---- lines --------------------------------------------------------------------------

    #[inline]
    fn line_start(&self, pos: usize) -> usize {
        memrchr(b'\n', &self.bytes[..pos]).map_or(0, |n| n + 1)
    }

    #[inline]
    fn line_end(&self, pos: usize) -> usize {
        memchr(b'\n', &self.bytes[pos..]).map_or(self.bytes.len(), |n| pos + n)
    }

    /// The comment walk of clones.rs's `clone_comment`, from the line that starts at `start`
    /// up, `budget` lines at most: the first line that decides, and how far up it is. A comment
    /// line with the mark decides it; a comment line without goes on; a line of code with a
    /// comment of its own, an empty line, or one ending in `;`, `{` or `}` stops; any other
    /// line of code (a statement's unended line) goes on. At the item's first line the walk
    /// goes on in the parent's text, which `above` says.
    fn walk_up(&self, start: usize, budget: u8, mark: fn(&str) -> Option<Mark>, above: Walk) -> Walk {
        let mut line_start = start;
        let mut k = 1u8;
        // Bounded: `budget` turns at most.
        while k <= budget {
            if line_start == 0 {
                let consumed = k - 1;
                return if above.dist != 0 && above.dist + consumed <= budget {
                    Walk { dist: above.dist + consumed, mark: above.mark }
                } else {
                    Walk::NONE
                };
            }
            let end = line_start - 1;
            let from = self.line_start(end);
            line_start = from;
            let line = self.text[from..end].trim();
            if line.starts_with("//") {
                if let Some(found) = mark(line) {
                    return Walk { dist: k, mark: found };
                }
            } else {
                let code = line.split("//").next().unwrap_or("").trim_end();
                if code.len() < line.len() || code.is_empty() || code.ends_with([';', '{', '}']) {
                    return Walk { dist: k, mark: Mark::Stop };
                }
            }
            k += 1;
        }
        Walk::NONE
    }

    /// The mark of the site at `pos`: on its own line, after the item on its last line, or
    /// above it by [`Pass::walk_up`].
    fn look(&self, pos: usize, mark: fn(&str) -> Option<Mark>, above: Walk, after: Option<Mark>) -> Option<Mark> {
        let start = self.line_start(pos);
        let end = self.line_end(pos);
        if let Some(found) = mark(&self.text[start..end]) {
            return Some(found);
        }
        if end == self.text.len()
            && let Some(found) = after
        {
            return Some(found);
        }
        let walk = self.walk_up(start, LOOK_UP, mark, above);
        (walk.dist != 0 && walk.mark != Mark::Stop).then_some(walk.mark)
    }

    /// How many lines above the line starting at `start` the nearest recursion mark is, within
    /// three; 0 for none.
    fn recursion_mark_above(&self, start: usize, above: u8) -> u8 {
        let mut line_start = start;
        let mut k = 1u8;
        // Bounded: three turns at most.
        while k <= RECURSION_LOOK_UP {
            if line_start == 0 {
                let consumed = k - 1;
                return if above != 0 && above + consumed <= RECURSION_LOOK_UP { above + consumed } else { 0 };
            }
            let end = line_start - 1;
            let from = self.line_start(end);
            line_start = from;
            if self.text[from..end].trim_start().starts_with(RECURSION_MARK) {
                return k;
            }
            k += 1;
        }
        0
    }

    /// Whether only spaces are before `pos` on its line.
    fn first_on_line(&self, pos: usize) -> bool {
        self.bytes[self.line_start(pos)..pos].iter().all(u8::is_ascii_whitespace)
    }

    // ---- scope --------------------------------------------------------------------------

    #[inline]
    fn innermost(&self) -> Block {
        self.frames.last().map_or(self.scope.block, |f| f.block)
    }

    fn in_module(&self) -> bool {
        self.scope.in_module || self.frames.iter().any(|f| f.block == Block::Module)
    }

    /// Whether the nearest function body or container is a container (recursion.rs's `assoc`).
    fn assoc_now(&self) -> bool {
        self.frames
            .iter()
            .rev()
            .find(|f| f.block == Block::Fn || f.block.container())
            .map_or(self.scope.assoc, |f| f.block.container())
    }

    #[inline]
    fn push_region(&mut self, span: Span, flavor: TestBits) {
        let max_end = self.regions.last().map_or(0, |r| r.max_end).max(span.end);
        self.regions.push(TestRegion { span, flavor, max_end });
    }

    #[inline]
    fn test_at(&self, pos: u32) -> TestBits {
        let mut bits = self.scope.test;
        if self.regions.is_empty() || bits == TestBits::ANY {
            return bits;
        }
        let mut j = self.regions.partition_point(|r| r.span.start <= pos);
        // Bounded: walks backwards at most `j` entries, stopping as soon as `max_end <= pos`.
        while j > 0 {
            j -= 1;
            let r = self.regions[j];
            if r.max_end <= pos {
                break;
            }
            if pos < r.span.end {
                bits = bits.union(r.flavor);
                if bits == TestBits::ANY {
                    break;
                }
            }
        }
        bits
    }

    // ---- the walk -----------------------------------------------------------------------

    fn walk(&mut self) {
        let n = self.toks.len();
        let mut i = 0;
        // Bounded: one turn per token.
        while i < n {
            let t = self.toks[i];
            match t.k {
                K::Hole => {
                    self.hole(t);
                    self.defining = false;
                }
                K::P(b'{') => self.open_brace(),
                K::P(b'}') => {
                    if let Some(frame) = self.frames.pop()
                        && frame.block == Block::Fn
                    {
                        self.bodies.pop();
                    }
                }
                K::P(b';') => {
                    self.in_use = false;
                    self.header = None;
                    if self.sig.is_some_and(|s| s.depth == 0) {
                        self.sig = None;
                    }
                }
                K::P(b'(') => self.deeper(),
                K::P(b'[') => {
                    self.deeper();
                    if self.rules.runs(Rule::Stops) {
                        self.cost.add(Rule::Stops);
                        self.index(i);
                    }
                }
                K::P(b')' | b']') => {
                    if let Some(sig) = &mut self.sig {
                        sig.depth = sig.depth.saturating_sub(1);
                    }
                }
                K::P(b'#') => self.attribute(i, t),
                K::P(b'&') => {
                    if self.rules.runs(Rule::MutRef) {
                        self.cost.add(Rule::MutRef);
                        self.mut_ref_amp(i, t);
                    }
                    if self.rules.runs(Rule::MagicSliceLen) {
                        self.cost.add(Rule::MagicSliceLen);
                        self.magic_slice_amp_index(i, t);
                    }
                }
                K::Ident => self.on_word(i, t),
                K::Raw => self.defining = false,
                K::Lit => {
                    if !self.scope.in_hermetic && self.rules.runs(Rule::RawSandboxExec) {
                        self.cost.add(Rule::RawSandboxExec);
                        self.raw_sandbox_exec(t);
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    #[inline]
    fn deeper(&mut self) {
        if let Some(sig) = &mut self.sig {
            sig.depth += 1;
        }
    }

    fn open_brace(&mut self) {
        let block = match self.sig {
            Some(sig) if sig.depth == 0 => {
                self.sig = None;
                self.bodies.push(sig.item);
                Block::Fn
            }
            // A brace inside a signature's brackets (a const generic's block): not its body.
            Some(_) => Block::Other,
            None => self.header.unwrap_or(Block::Other),
        };
        self.header = None;
        self.frames.push(Frame { block });
    }

    /// A child item: its scope, from where it is in this item's text.
    fn hole(&mut self, t: Tok) {
        let parent = self.scope.context;
        let line = self.line_start(t.start as usize);
        let after_end = self.line_end(t.end as usize);
        let rest = &self.text[t.end as usize..after_end];
        let at_end = after_end == self.text.len();
        let context = crate::Context {
            clone_above: self.walk_up(line, LOOK_UP, clone_mark, parent.clone_above),
            clone_after: clone_mark(rest).or(if at_end { parent.clone_after } else { None }),
            safety_above: self.walk_up(line, LOOK_UP, safety_mark, parent.safety_above),
            safety_after: safety_mark(rest).is_some() || (at_end && parent.safety_after),
            seqcst_above: self.walk_up(line, LOOK_UP, seqcst_mark, parent.seqcst_above),
            seqcst_after: seqcst_mark(rest).is_some() || (at_end && parent.seqcst_after),
            recursion_above: self.recursion_mark_above(line, parent.recursion_above),
        };
        let scope = Scope {
            test: self.test_at(t.start),
            block: self.innermost(),
            assoc: self.assoc_now(),
            in_module: self.in_module(),
            bin_root: self.scope.bin_root,
            build_script: self.scope.build_script,
            sans_io: self.scope.sans_io,
            own_heads: self.scope.own_heads,
            statics: self.scope.statics,
            lock_reason: self.scope.lock_reason || self.reasoned(t.start),
            trust_unsafe: self.scope.trust_unsafe,
            in_hermetic: self.scope.in_hermetic,
            context,
        };
        self.children.push(scope);
    }

    fn on_word(&mut self, i: usize, t: Tok) {
        let word_id = t.word;
        if word_id == NO_WORD {
            // No rule reads this word (every word below is in the table, a compile-time check
            // in sequences.rs): only a recursive call, `match` discriminant table, or
            // `#[proc_macro*]` attribute can be its use.
            self.defining = false;
            let len = t.end - t.start;
            if len == 5 && self.rules.runs(Rule::HandWrittenDerivable) && self.str_of(t) == "match" {
                self.check_hand_written_discriminant(i, t);
            } else if matches!(len, 10 | 17 | 20)
                && self.rules.runs(Rule::ProcMacroDef)
                && matches!(self.str_of(t), "proc_macro" | "proc_macro_derive" | "proc_macro_attribute")
                && self.back(i, 1).is_some_and(|b| b.k == K::P(b'['))
                && self.back(i, 2).is_some_and(|b| b.k == K::P(b'#'))
            {
                self.push(Template::ProcMacroDef, self.toks[i - 2].start, t.end, None);
            }
            if self.rules.runs(Rule::Recursion) {
                self.cost.add(Rule::Recursion);
                if self.sig.is_none() && !self.bodies.is_empty() {
                    self.recursion_name(i, t);
                }
            }
            return;
        }
        let class = WORD_CLASS[word_id as usize];
        let was_defining = core::mem::replace(&mut self.defining, class & WC_DEFINING != 0);
        let rules = self.rules;
        if class & WC_DISPATCHED != 0 {
            match word_id {
                W_USE => self.in_use = true,
                W_AS => {
                    if rules.runs(Rule::Casts) {
                        self.cost.add(Rule::Casts);
                        self.cast(i, t);
                    }
                    if rules.runs(Rule::BoolAsInt) {
                        self.cost.add(Rule::BoolAsInt);
                        self.bool_as_int(i, t);
                    }
                }
                W_CLONE if rules.runs(Rule::Clones) => {
                    self.cost.add(Rule::Clones);
                    self.clone_call(i, t);
                }
                W_GLOBAL if rules.runs(Rule::Allocs) && !self.in_use => {
                    self.cost.add(Rule::Allocs);
                    self.push(Template::AllocGlobal, t.start, t.end, None);
                }
                W_FN => {
                    self.fn_word(i, t);
                    // `fn_word` reads the signature for each of these rules that runs.
                    for rule in [Rule::Allocs, Rule::Untyped, Rule::Recursion] {
                        if rules.runs(rule) {
                            self.cost.add(rule);
                        }
                    }
                }
                W_TYPE => {
                    self.type_word(i, t);
                    if rules.runs(Rule::Untyped) {
                        self.cost.add(Rule::Untyped);
                    }
                }
                W_IMPL | W_TRAIT | W_MOD if self.sig.is_none() => {
                    self.header = Some(match word_id {
                        W_IMPL => Block::Impl,
                        W_TRAIT => Block::Trait,
                        _ => Block::Module,
                    });
                }
                W_FOR => {
                    if self.header == Some(Block::Impl) && !matches!(self.tok(i + 1), Some(n) if n.k == K::P(b'<')) {
                        self.header = Some(Block::TraitImpl);
                    } else if rules.runs(Rule::ManualSliceIdioms) {
                        self.cost.add(Rule::ManualSliceIdioms);
                        self.manual_index_loop(i, t);
                    }
                }
                W_SELF => {
                    if let Some(sig) = self.sig
                        && sig.depth == 1
                    {
                        self.fns[sig.item as usize].has_self = true;
                    }
                }
                W_UNWRAP | W_EXPECT if rules.runs(Rule::Stops) => {
                    self.cost.add(Rule::Stops);
                    self.stop(i, t, word_id);
                }
                W_STD | W_ALLOC if rules.runs(Rule::Ambient) => {
                    self.cost.add(Rule::Ambient);
                    self.ambient_head(i, t, word_id);
                }
                W_UNSAFE => {
                    if rules.runs(Rule::UnsafeComment) {
                        self.cost.add(Rule::UnsafeComment);
                        self.unsafe_word(i, t);
                    }
                    if !self.scope.trust_unsafe && rules.runs(Rule::WorkspaceUnsafe) {
                        self.cost.add(Rule::WorkspaceUnsafe);
                        self.workspace_unsafe_word(i, t);
                    }
                }
                W_ORDERING if rules.runs(Rule::SeqCstUnexplained) => {
                    self.cost.add(Rule::SeqCstUnexplained);
                    self.seqcst_word(i, t);
                }
                W_REF if rules.runs(Rule::MutRef) => {
                    self.cost.add(Rule::MutRef);
                    self.ref_mut_word(i, t);
                }
                W_LET if rules.runs(Rule::MutForUniqBorrow) => {
                    self.cost.add(Rule::MutForUniqBorrow);
                    self.let_mut_word(i, t);
                }
                W_GET if rules.runs(Rule::MagicSliceLen) => {
                    self.cost.add(Rule::MagicSliceLen);
                    self.magic_slice_get(i, t);
                }
                W_STARTS_WITH if rules.runs(Rule::MagicSliceLen) => {
                    self.cost.add(Rule::MagicSliceLen);
                    self.magic_slice_starts_with(i, t);
                }
                W_LEN if rules.runs(Rule::ManualSliceIdioms) => {
                    self.cost.add(Rule::ManualSliceIdioms);
                    self.manual_len_zero(i, t);
                }
                W_FIND if rules.runs(Rule::ManualSliceIdioms) => {
                    self.cost.add(Rule::ManualSliceIdioms);
                    self.manual_find_split(i, t);
                }
                W_ALLOC_TYPE if rules.runs(Rule::TestAmbientLeak) => {
                    self.cost.add(Rule::TestAmbientLeak);
                    self.test_global_alloc(i, t);
                }
                W_EXTERN if rules.runs(Rule::TestAmbientLeak) => {
                    self.cost.add(Rule::TestAmbientLeak);
                    self.test_extern_std(i, t);
                }
                _ => {}
            }
        }
        const EXTRA: u16 = WC_COLLECTION | WC_RENAME | WC_STATIC | WC_SANS_IO | WC_LOCK_TYPE;
        if class & EXTRA != 0 {
            if class & WC_COLLECTION != 0 && !self.in_use && !was_defining {
                if rules.runs(Rule::Ambient) {
                    self.cost.add(Rule::Ambient);
                    self.collection(i, t, word_id);
                }
                if word_id == W_ARC && rules.runs(Rule::DoubleIndirectionArc) {
                    self.cost.add(Rule::DoubleIndirectionArc);
                    self.double_indirection_arc(i, t);
                }
            }
            if class & WC_RENAME != 0 && rules.runs(Rule::Renamed) {
                self.cost.add(Rule::Renamed);
                self.renamed(i, t, self.str_of(t));
            }
            if class & WC_STATIC != 0 && rules.runs(Rule::ProcessStatic) {
                self.cost.add(Rule::ProcessStatic);
                self.process_static(i, t, word_id);
            }
            if class & WC_SANS_IO != 0 && self.scope.sans_io && rules.runs(Rule::SansIoCore) {
                self.cost.add(Rule::SansIoCore);
                self.sans_io_word(i, t, word_id, class);
            }
            if class & WC_LOCK_TYPE != 0 && rules.runs(Rule::LockWithoutReason) {
                self.cost.add(Rule::LockWithoutReason);
                self.lock_ctor(i, t, self.str_of(t));
            }
        }
        if rules.runs(Rule::Recursion) {
            self.cost.add(Rule::Recursion);
            if self.sig.is_none() && !self.bodies.is_empty() {
                self.recursion_name(i, t);
            }
        }
    }

    #[inline]
    fn push(&mut self, template: Template, start: u32, end: u32, arg: Option<Arg>) {
        self.cands.push(Cand { template, range: Span::new(start, end), arg, plan: Plan::None });
    }

    // ---- test code ----------------------------------------------------------------------

    /// A `#` that starts its line: a test attribute opens test code (the ratchet's patterns);
    /// an `allow` or `expect` of `blaze::implicit_allocator` is allocs.rs's `expect`.
    fn attribute(&mut self, i: usize, t: Tok) {
        let pos = t.start as usize;
        if self.rules.runs(Rule::LockWithoutReason) && self.lock_exception(i, t) {
            return;
        }
        if !self.first_on_line(pos) {
            return;
        }
        let rest = &self.text[pos..];
        if self.rules.runs(Rule::Allocs)
            && let Some(end) = expects_implicit_allocator(rest)
        {
            self.push(Template::AllocExpect, t.start, t.start + end as u32, None);
        }
        if cfg_test_end(rest, true).is_some() {
            // `#![cfg(test)]`: the rest of the item (the file, for the root) is test code.
            self.push_region(Span::new(t.start, self.text.len() as u32), TestBits::CFG);
            return;
        }
        let (end, flavor) = match cfg_test_end(rest, false) {
            Some(end) => (end, TestBits::CFG),
            None => match test_attr_end(rest) {
                Some(end) => (end, TestBits::ATTR),
                None => return,
            },
        };
        let after = t.start + end as u32;
        let region_end = match self.item_end(i, after) {
            ItemEnd::At(end) => end,
            // A closing bracket first (an attribute on a statement or an expression): to the
            // end of its line, or that bracket if sooner.
            ItemEnd::Under(bracket) => (self.line_end(pos) as u32).min(bracket),
            ItemEnd::Open => self.line_end(pos) as u32,
        };
        self.push_region(Span::new(t.start, region_end), flavor);
    }

    /// The ratchet's `item_end` from byte `from`: the end of the first `;` outside brackets, or
    /// of the `}` that closes the first brace.
    fn item_end(&self, i: usize, from: u32) -> ItemEnd {
        let mut j = i;
        // Bounded: each turn moves `j` forward.
        while matches!(self.tok(j), Some(t) if t.start < from) {
            j += 1;
        }
        let mut depth = 0usize;
        // Bounded: each turn moves `j` forward, to the last token at most.
        while let Some(t) = self.tok(j) {
            match t.k {
                K::P(b'{' | b'(' | b'[') => depth += 1,
                K::P(c @ (b'}' | b')' | b']')) => {
                    if depth == 0 {
                        return ItemEnd::Under(t.start);
                    }
                    depth -= 1;
                    if c == b'}' && depth == 0 {
                        return ItemEnd::At(t.end);
                    }
                }
                K::P(b';') if depth == 0 => return ItemEnd::At(t.end),
                _ => {}
            }
            j += 1;
        }
        ItemEnd::Open
    }

    // ---- casts --------------------------------------------------------------------------

    /// casts.rs: `as` (not in a `use`) then a primitive integer type's name, not a path's
    /// start (`u32::MAX`).
    fn cast(&mut self, i: usize, t: Tok) {
        if self.in_use {
            return;
        }
        let Some(ty) = self.tok(i + 1) else { return };
        if ty.k != K::Ident || !INTEGERS.contains(&self.str_of(ty)) {
            return;
        }
        if matches!(self.tok(i + 2), Some(c) if c.k == K::P(b':') && c.start == ty.end) && self.colons_at(i + 2) {
            return;
        }
        self.push(Template::CastToInt, t.start, ty.end, Some(Arg::Span(Span::new(ty.start, ty.end))));
    }

    // ---- clones -------------------------------------------------------------------------

    /// clones.rs: `.clone()` (not `..clone`), or `Clone::clone(`; its kind by its comment,
    /// then by its receiver's declaration in this item.
    fn clone_call(&mut self, i: usize, t: Tok) {
        let next = self.tok(i + 1);
        let receiver = if self.method(i) {
            let closes = matches!(next, Some(n) if n.k == K::P(b'('))
                && matches!(self.tok(i + 2), Some(n) if n.k == K::P(b')'));
            if !closes {
                return;
            }
            self.receiver(i - 1)
        } else if self.colons_before(i) && self.is_word(self.back(i, 3), "Clone") {
            if !matches!(next, Some(n) if n.k == K::P(b'(')) {
                return;
            }
            None
        } else {
            return;
        };
        let handle_kind = match receiver {
            // A field's declaration is the file's, which an item does not see: its own
            // text is read (clones.rs reads the whole file).
            Some(Receiver { name, is_field: true }) => self.declared_handle(0, self.toks.len(), name, false),
            Some(Receiver { name, is_field: false }) => self.declared_handle(self.last_fn.unwrap_or(0), i, name, true),
            None => None,
        };
        let context = self.scope.context;
        let template = match self.look(t.start as usize, clone_mark, context.clone_above, context.clone_after) {
            Some(Mark::Handle) => Template::CloneHandle,
            Some(Mark::ErrorPath) => Template::CloneErrorPath,
            Some(Mark::Deep) => Template::CloneDeep,
            _ => {
                if handle_kind.is_some() {
                    Template::CloneHandle
                } else {
                    Template::CloneUnknown
                }
            }
        };
        let is_method = self.method(i);
        let end = if is_method { self.tok(i + 2).map_or(t.end, |c| c.end) } else { t.end };
        let plan = if is_method && template == Template::CloneHandle {
            match handle_kind {
                Some(HandleKind::Local) => Plan::Replace { range: Span::new(t.start, t.end), text: "local_share" },
                Some(HandleKind::Shared) => Plan::Replace { range: Span::new(t.start, t.end), text: "share" },
                None => Plan::None,
            }
        } else {
            Plan::None
        };
        self.cands.push(Cand { template, range: Span::new(t.start, end), arg: None, plan });
    }

    /// clones.rs's `receiver_of`: the last name of the run of names and dots before the `.`
    /// at `dot`, and whether it is a field (a dot in the run, or a `)` or `]` before it).
    fn receiver(&self, dot: usize) -> Option<Receiver<'u>> {
        let mut j = dot.checked_sub(1)?;
        let last = self.tok(j)?;
        let name = match last.k {
            K::Ident => self.str_of(last),
            K::Raw => &self.str_of(last)[2..],
            _ => return None,
        };
        let mut run_start = j;
        if last.k == K::Ident {
            // Bounded: each turn moves `j` back by one token.
            while j > 0 {
                let (Some(before), Some(here)) = (self.tok(j - 1), self.tok(j)) else { break };
                if before.end != here.start || !matches!(before.k, K::Ident | K::Int | K::Float | K::P(b'.')) {
                    break;
                }
                j -= 1;
                run_start = j;
            }
        }
        let is_field = run_start < dot - 1 || matches!(self.back(run_start, 1), Some(b) if matches!(b.k, K::P(b')' | b']')));
        Some(Receiver { name, is_field })
    }

    /// clones.rs's `declared_handle` over tokens `from..to`: `name: [&][mut] path::Arc<` or
    /// `Rc<`, and with `assigned`, `name = path::Arc::..` or `Rc::..`.
    fn declared_handle(&self, from: usize, to: usize, name: &str, assigned: bool) -> Option<HandleKind> {
        let mut k = from;
        // Bounded: one turn per token of the range.
        while k < to {
            if self.is_word(self.tok(k), name) {
                let next = self.tok(k + 1);
                if matches!(next, Some(n) if n.k == K::P(b':')) && !self.colons_at(k + 1) {
                    if let Some(kind) = self.handle_type(k + 2) {
                        return Some(kind);
                    }
                } else if assigned
                    && let Some(eq) = next
                    && eq.k == K::P(b'=')
                    && !matches!(self.tok(k + 2), Some(n) if matches!(n.k, K::P(b'=' | b'>')) && n.start == eq.end)
                    && let Some(kind) = self.handle_value(k + 2)
                {
                    return Some(kind);
                }
            }
            k += 1;
        }
        None
    }

    fn handle_type(&self, mut k: usize) -> Option<HandleKind> {
        // Bounded: each turn moves `k` forward past one `&`.
        while matches!(self.tok(k), Some(t) if t.k == K::P(b'&')) {
            k += 1;
        }
        if self.is_word(self.tok(k), "mut") {
            k += 1;
        }
        let last = self.path_end(k)?;
        if matches!(self.tok(last + 1), Some(t) if t.k == K::P(b'<')) {
            classify_handle(self.word(self.tok(last)).unwrap_or(""))
        } else {
            None
        }
    }

    fn handle_value(&self, k: usize) -> Option<HandleKind> {
        let last = self.path_end(k)?;
        // `Arc::<T>::default()`: the path stops at a turbofish, its last word the type (as
        // clones.rs's `path_run` stops at the `<`).
        if self.colons_at(last + 1) {
            return classify_handle(self.word(self.tok(last)).unwrap_or(""));
        }
        if last >= k + 3 {
            classify_handle(self.word(self.tok(last - 3)).unwrap_or(""))
        } else {
            None
        }
    }

    /// The last token of the path `name (:: name)*` starting at `k`.
    fn path_end(&self, k: usize) -> Option<usize> {
        self.word(self.tok(k))?;
        let mut last = k;
        // Bounded: each turn moves `last` forward by one segment.
        while self.colons_at(last + 1) && self.word(self.tok(last + 3)).is_some() {
            last += 3;
        }
        Some(last)
    }

    // ---- fn and type: allocs, untyped, recursion ----------------------------------------

    fn fn_word(&mut self, i: usize, t: Tok) {
        self.last_fn = Some(i);
        let name = self.tok(i + 1);
        let named = match name {
            Some(n) if n.k == K::Ident => Some(self.str_of(n)),
            Some(n) if n.k == K::Raw => Some(&self.str_of(n)[2..]),
            _ => None,
        };
        let in_trait_impl = self.innermost() == Block::TraitImpl;
        if let (Some(name), Some(name_tok)) = (named, name)
            && !in_trait_impl
        {
            let rules = self.rules;
            let close = self.signature_end(i + 1);
            if rules.runs(Rule::Allocs)
                && name != "main"
                && let Some(close) = close
                && let Some(ty) = self.fixes_allocator(i + 1, close)
            {
                self.push(Template::AllocSignature, t.start, name_tok.end, Some(Arg::Span(ty)));
            }
            let main_module = self.scope.bin_root && !self.in_module();
            if rules.runs(Rule::Untyped)
                && !main_module
                && !self.scope.build_script
                && let Some(close) = close
            {
                let found = self.untyped_errors(i + 1, close);
                const KINDS: [Template; 4] =
                    [Template::UntypedString, Template::UntypedStr, Template::UntypedBoxed, Template::UntypedAnyhow];
                for (kind, span) in KINDS.iter().zip(found) {
                    if let Some(span) = span {
                        self.push(*kind, t.start, name_tok.end, Some(Arg::Span(span)));
                    }
                }
            }
            if (rules.runs(Rule::BoolParam)
                || rules.runs(Rule::SwappableParameters)
                || rules.runs(Rule::PrimitiveMeaning))
                && let Some(close) = close
                && self.is_pub_fn(i)
            {
                self.check_newtype_params(i + 1, close, name);
            }
            if rules.runs(Rule::AnonTupleReturn)
                && let Some(close) = close
            {
                self.check_anon_tuple_return(t.start, name_tok.end, i + 1, close);
            }
        }
        if self.sig.is_none()
            && let Some(n) = name
            && n.k == K::Ident
        {
            let marked = self.rules.runs(Rule::Recursion)
                && self.recursion_mark_above(self.line_start(t.start as usize), self.scope.context.recursion_above) != 0;
            let item = self.fns.len() as u32;
            self.fns.push(FnRec { at: i as u32, name: Span::new(n.start, n.end), assoc: self.assoc_now(), has_self: false, recursive: false, marked });
            self.sig = Some(Sig { item, depth: 0 });
        }
    }

    /// untyped.rs's alias kind: `type X = <a Result with an untyped error>;`.
    fn type_word(&mut self, i: usize, t: Tok) {
        if !self.rules.runs(Rule::Untyped) || (self.scope.bin_root && !self.in_module()) || self.scope.build_script {
            return;
        }
        let Some(close) = self.signature_end(i + 1) else { return };
        let Some(eq) = self.alias_eq(i + 1, close) else { return };
        if self.untyped_errors(eq + 1, close).iter().any(Option::is_some) {
            let (Some(first), Some(last)) = (self.tok(eq + 1), self.back(close, 1)) else { return };
            let name_end = self.tok(i + 1).map_or(t.end, |n| n.end);
            self.push(Template::UntypedAlias, t.start, name_end, Some(Arg::Span(Span::new(first.start, last.end))));
        }
    }

    /// The `{` or `;` that ends the signature from token `from`: the first outside `(`, `[`,
    /// `<` and `{` (the `>` of `->` closes none); `None` when a bracket closes that it did not
    /// open.
    fn signature_end(&self, from: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut j = from;
        // Bounded: each turn moves `j` forward, to the last token at most.
        while let Some(t) = self.tok(j) {
            match t.k {
                K::P(b'{' | b';') if depth == 0 => return Some(j),
                K::P(b'(' | b'[' | b'<' | b'{') => depth += 1,
                K::P(b'>') if self.arrow(j) => {}
                K::P(b')' | b']' | b'>' | b'}') => depth = depth.checked_sub(1)?,
                _ => {}
            }
            j += 1;
        }
        None
    }

    /// The `>` at `j` is `->`'s.
    #[inline]
    fn arrow(&self, j: usize) -> bool {
        matches!((self.back(j, 1), self.tok(j)), (Some(m), Some(g)) if m.k == K::P(b'-') && m.end == g.start)
    }

    /// Whether the `fn` at token `fn_at` has a `pub` visibility modifier immediately preceding it.
    fn is_pub_fn(&self, fn_at: usize) -> bool {
        let mut j = fn_at;
        let mut steps = 0usize;
        while j > 0 && steps < 10 {
            j -= 1;
            steps += 1;
            let Some(t) = self.tok(j) else { break };
            match t.k {
                K::Ident => match self.str_of(t) {
                    "pub" => return true,
                    "const" | "async" | "unsafe" | "extern" => continue,
                    _ => return false,
                },
                K::Lit => continue,
                K::P(b')') => {
                    let mut depth = 1usize;
                    while j > 0 && depth > 0 {
                        j -= 1;
                        if let Some(p) = self.tok(j) {
                            match p.k {
                                K::P(b')') => depth += 1,
                                K::P(b'(') => depth -= 1,
                                _ => {}
                            }
                        }
                    }
                }
                _ => return false,
            }
        }
        false
    }

    /// Checks a `pub fn`'s parameter list (`from..close`) for `Rule::BoolParam`,
    /// `Rule::SwappableParameters`, and `Rule::PrimitiveMeaning` in one zero-allocation pass.
    fn check_newtype_params(&mut self, from: usize, close: usize, fn_name: &str) {
        let mut depth = 0usize;
        let mut open_paren = None;
        let mut j = from;
        while j < close {
            if let Some(t) = self.tok(j) {
                match t.k {
                    K::P(b'(') if depth == 0 => {
                        open_paren = Some(j);
                        break;
                    }
                    K::P(b'<' | b'[' | b'{') => depth += 1,
                    K::P(b'>' | b']' | b'}') => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
            j += 1;
        }
        let Some(lparen) = open_paren else { return };

        let mut rparen = None;
        let mut pdepth = 1usize;
        let mut k = lparen + 1;
        while k < close {
            if let Some(t) = self.tok(k) {
                match t.k {
                    K::P(b'(') => pdepth += 1,
                    K::P(b')') => {
                        pdepth -= 1;
                        if pdepth == 0 {
                            rparen = Some(k);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            k += 1;
        }
        let Some(rparen) = rparen else { return };

        let mut params: [ParsedParam; 16] = [ParsedParam {
            name_span: Span::new(0, 0),
            ty_span: Span::new(0, 0),
            prim: None,
            is_numeric: false,
        }; 16];
        let mut nparams = 0usize;

        let mut seg_start = lparen + 1;
        let mut bdepth = 0usize;
        let mut idx = lparen + 1;
        while idx <= rparen {
            let at_end = idx == rparen;
            let is_comma = !at_end && self.tok(idx).is_some_and(|t| t.k == K::P(b',') && bdepth == 0);
            if !at_end {
                if let Some(t) = self.tok(idx) {
                    match t.k {
                        K::P(b'(' | b'[' | b'<' | b'{') => bdepth += 1,
                        K::P(b')' | b']' | b'>' | b'}') => bdepth = bdepth.saturating_sub(1),
                        _ => {}
                    }
                }
            }
            if (is_comma || at_end) && seg_start < idx && nparams < params.len() {
                if let Some(parsed) = self.parse_param_slice(seg_start, idx) {
                    params[nparams] = parsed;
                    nparams += 1;
                }
                seg_start = idx + 1;
            }
            idx += 1;
        }

        let rules = self.rules;
        let trivial_bool_setter = nparams == 1 && (fn_name.starts_with("set_") || fn_name == "from_bool");

        for p_idx in 0..nparams {
            let p = params[p_idx];
            if rules.runs(Rule::BoolParam) && p.prim == Some(PrimKind::Bool) && !trivial_bool_setter {
                self.push(
                    Template::BoolParam,
                    p.name_span.start,
                    p.ty_span.end,
                    Some(Arg::Span(p.name_span)),
                );
            }
            if rules.runs(Rule::PrimitiveMeaning) && p.is_numeric {
                let pname = p.name_span.of(self.text);
                if carries_primitive_meaning(pname) {
                    self.push(
                        Template::PrimitiveMeaning,
                        p.name_span.start,
                        p.ty_span.end,
                        Some(Arg::Span(p.ty_span)),
                    );
                }
            }
            if rules.runs(Rule::SwappableParameters) && p_idx + 1 < nparams {
                let next = params[p_idx + 1];
                if let (Some(k1), Some(k2)) = (p.prim, next.prim)
                    && k1 == k2
                {
                    let n1 = p.name_span.of(self.text);
                    let n2 = next.name_span.of(self.text);
                    if !is_symmetric_param_pair(n1, n2) {
                        self.push(
                            Template::SwappableParameters,
                            p.name_span.start,
                            next.ty_span.end,
                            Some(Arg::Span(p.ty_span)),
                        );
                    }
                }
            }
        }
    }

    fn parse_param_slice(&self, from: usize, to: usize) -> Option<ParsedParam> {
        let mut j = from;
        while j < to {
            let t = self.tok(j)?;
            if t.k == K::P(b'#') {
                j += 1;
                if self.tok(j).is_some_and(|b| b.k == K::P(b'[')) {
                    let mut d = 1usize;
                    j += 1;
                    while j < to && d > 0 {
                        if let Some(tok) = self.tok(j) {
                            match tok.k {
                                K::P(b'[') => d += 1,
                                K::P(b']') => d -= 1,
                                _ => {}
                            }
                        }
                        j += 1;
                    }
                }
                continue;
            }
            if t.k == K::Ident && self.str_of(t) == "mut" {
                j += 1;
                continue;
            }
            break;
        }
        let name_tok = self.tok(j)?;
        if name_tok.k != K::Ident {
            return None;
        }
        let name_str = self.str_of(name_tok);
        if name_str == "self" || name_str.starts_with('_') {
            return None;
        }
        let colon = self.tok(j + 1)?;
        if colon.k != K::P(b':') {
            return None;
        }
        let ty_from = j + 2;
        if ty_from >= to {
            return None;
        }
        let first_ty = self.tok(ty_from)?;
        let last_ty = self.back(to, 1)?;
        let ty_span = Span::new(first_ty.start, last_ty.end);

        let class = if ty_from + 1 == to && first_ty.k == K::Ident {
            classify_primitive(self.str_of(first_ty))
        } else if first_ty.k == K::P(b'&') && last_ty.k == K::Ident && self.str_of(last_ty) == "str" {
            let len = to - ty_from;
            if len == 2 || (len == 3 && self.tok(ty_from + 1).is_some_and(|t| t.k == K::Life)) {
                PrimClass { kind: Some(PrimKind::Str), is_numeric: false }
            } else {
                PrimClass::NONE
            }
        } else {
            PrimClass::NONE
        };
        Some(ParsedParam {
            name_span: Span::new(name_tok.start, name_tok.end),
            ty_span,
            prim: class.kind,
            is_numeric: class.is_numeric,
        })
    }

    /// Checks a function's return type (`-> ...` inside `from..close`) for anonymous tuples
    /// carrying unnamed state such as `(Option<T>, bool)`, `(T, bool)`, `Option<(A, B, C)>`, or
    /// `Result<(A, bool), E>`.
    fn check_anon_tuple_return(&mut self, fn_start: u32, name_end: u32, from: usize, close: usize) {
        // Find top-level `->` after the parameter list.
        let mut depth = 0usize;
        let mut ret_start = None;
        let mut j = from;
        while j < close {
            if let Some(t) = self.tok(j) {
                match t.k {
                    K::P(b'(' | b'[' | b'<' | b'{') => depth += 1,
                    K::P(b'>') if self.arrow(j) && depth == 0 => {
                        ret_start = Some(j + 1);
                        break;
                    }
                    K::P(b')' | b']' | b'>' | b'}') => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
            j += 1;
        }
        let Some(ret_from) = ret_start else { return };

        // Stop at top-level `where`.
        let mut ret_end = close;
        let mut d = 0usize;
        let mut k = ret_from;
        while k < close {
            if let Some(t) = self.tok(k) {
                match t.k {
                    K::P(b'(' | b'[' | b'<' | b'{') => d += 1,
                    K::P(b')' | b']' | b'>' | b'}') => d = d.saturating_sub(1),
                    K::Ident if d == 0 && self.str_of(t) == "where" => {
                        ret_end = k;
                        break;
                    }
                    _ => {}
                }
            }
            k += 1;
        }
        if ret_from >= ret_end {
            return;
        }

        // Unwrap leading `Option<...>` or `Result<..., E>` wrapper (or qualified `core::option::Option<...>`).
        let mut tuple_open = None;
        let Some(first) = self.tok(ret_from) else { return };
        if first.k == K::P(b'(') {
            tuple_open = Some(ret_from);
        } else {
            // Scan optional path prefix ending in `Option` or `Result` followed by `<(`.
            let mut p = ret_from;
            while p + 2 < ret_end {
                let Some(tok) = self.tok(p) else { break };
                if tok.k == K::Ident
                    && matches!(self.str_of(tok), "Option" | "Result")
                    && self.tok(p + 1).is_some_and(|x| x.k == K::P(b'<'))
                    && self.tok(p + 2).is_some_and(|x| x.k == K::P(b'('))
                {
                    tuple_open = Some(p + 2);
                    break;
                }
                if tok.k == K::Ident || tok.k == K::P(b':') {
                    p += 1;
                } else {
                    break;
                }
            }
        }
        let Some(open) = tuple_open else { return };

        // Scan the tuple `( elem_0, elem_1, ... )` at paren depth 1.
        let mut tdepth = 1usize;
        let mut elem_count = 0usize;
        let mut has_token_in_elem = false;
        let mut has_state_type = false;
        let mut tuple_close = None;
        let mut m = open + 1;
        while m < ret_end {
            let Some(tok) = self.tok(m) else { break };
            match tok.k {
                K::P(b'(' | b'[' | b'<' | b'{') => {
                    tdepth += 1;
                    has_token_in_elem = true;
                }
                K::P(b')') if tdepth == 1 => {
                    if has_token_in_elem {
                        elem_count += 1;
                    }
                    tuple_close = Some(m);
                    break;
                }
                K::P(b')' | b']' | b'>' | b'}') => {
                    tdepth = tdepth.saturating_sub(1);
                    has_token_in_elem = true;
                }
                K::P(b',') if tdepth == 1 => {
                    if has_token_in_elem {
                        elem_count += 1;
                        has_token_in_elem = false;
                    }
                }
                K::Ident => {
                    has_token_in_elem = true;
                    if tdepth == 1 && matches!(self.str_of(tok), "bool" | "Option" | "Result") {
                        has_state_type = true;
                    }
                }
                _ => {
                    has_token_in_elem = true;
                }
            }
            m += 1;
        }
        let Some(close_tok_idx) = tuple_close else { return };

        // Flag any tuple with 2+ elements carrying unnamed state (`bool` / `Option` / `Result`)
        // OR any tuple with 3+ elements.
        if elem_count >= 2 && (has_state_type || elem_count >= 3) {
            let Some(open_tok) = self.tok(open) else { return };
            let Some(close_tok) = self.tok(close_tok_idx) else { return };
            let tuple_span = Span::new(open_tok.start, close_tok.end);
            self.push(
                Template::AnonTupleReturn,
                fn_start,
                name_end,
                Some(Arg::Span(tuple_span)),
            );
        }
    }

    /// Detects `match expr { Variant => <int>, ... }` with `>= 8` arms mapping directly to
    /// integer literals (a hand-written discriminant table where `#[repr(u8)]`,
    /// `core::mem::discriminant`, or `bstd::comptime` derive belongs).
    fn check_hand_written_discriminant(&mut self, match_at: usize, match_tok: Tok) {
        // Find opening `{` of the `match` body at paren/bracket depth 0.
        let mut depth = 0usize;
        let mut open = None;
        let mut j = match_at + 1;
        let scan_limit = (match_at + 48).min(self.toks.len());
        while j < scan_limit {
            if let Some(t) = self.tok(j) {
                match t.k {
                    K::P(b'(' | b'[') => depth += 1,
                    K::P(b')' | b']') => depth = depth.saturating_sub(1),
                    K::P(b'{') if depth == 0 => {
                        open = Some(j);
                        break;
                    }
                    K::P(b';') if depth == 0 => return,
                    _ => {}
                }
            }
            j += 1;
        }
        let Some(open) = open else { return };

        // Count top-level `=> <int_literal>` arms inside `{ ... }`.
        let mut bdepth = 1usize;
        let mut int_arms = 0u32;
        let mut total_arms = 0u32;
        let mut k = open + 1;
        while k < self.toks.len() && bdepth > 0 {
            let Some(t) = self.tok(k) else { break };
            match t.k {
                K::P(b'{') => bdepth += 1,
                K::P(b'}') => bdepth -= 1,
                K::P(b'=')
                    if bdepth == 1
                        && self.tok(k + 1).is_some_and(|g| g.k == K::P(b'>') && t.end == g.start) =>
                {
                    total_arms += 1;
                    // Check if arm body is a bare integer literal followed by `,` or `}`.
                    if self.tok(k + 2).is_some_and(|lit| lit.k == K::Int)
                        && self
                            .tok(k + 3)
                            .is_some_and(|after| matches!(after.k, K::P(b',' | b'}')))
                    {
                        int_arms += 1;
                    }
                    k += 1;
                }
                _ => {}
            }
            k += 1;
        }
        if bdepth == 0 && total_arms >= 8 && int_arms >= 8 && int_arms * 4 >= total_arms * 3 {
            self.push(
                Template::HandWrittenDiscriminant,
                match_tok.start,
                match_tok.end,
                Some(Arg::Int(int_arms)),
            );
        }
    }

    /// Flags `Arc<Vec<...>>` and `Arc<String>` (including path-qualified forms and optional
    /// allocator parameters) in type syntax, advising `blaze_own::ThinArc` / `blaze_own::ThinStr`.
    fn double_indirection_arc(&mut self, i: usize, arc_tok: Tok) {
        if self.method(i) || !matches!(self.tok(i + 1), Some(t) if t.k == K::P(b'<')) {
            return;
        }
        let mut k = i + 2;
        if self.colons_at(k) {
            k += 2;
        }
        let Some(last) = self.path_end(k) else { return };
        let inner_tok = self.toks[last];
        let limit = (i + 128).min(self.toks.len());
        let after_inner = match inner_tok.word {
            W_VEC => {
                if !matches!(self.tok(last + 1), Some(t) if t.k == K::P(b'<')) {
                    return;
                }
                let Some(vec_close) = self.closing_angle_from(last + 1, 0, limit) else { return };
                vec_close + 1
            }
            W_STRING => match self.tok(last + 1).map(|t| t.k) {
                Some(K::P(b'>' | b',')) => last + 1,
                Some(K::P(b'<')) => {
                    let Some(str_close) = self.closing_angle_from(last + 1, 0, limit) else { return };
                    str_close + 1
                }
                _ => return,
            },
            _ => return,
        };
        let arc_close = match self.tok(after_inner).map(|t| t.k) {
            Some(K::P(b'>')) => after_inner,
            Some(K::P(b',')) => {
                let Some(close) = self.closing_angle_from(after_inner + 1, 1, limit) else { return };
                close
            }
            _ => return,
        };
        let end = self.toks[arc_close].end;
        self.push(
            Template::DoubleIndirectionArc,
            arc_tok.start,
            end,
            Some(Arg::Span(Span::new(inner_tok.start, inner_tok.end))),
        );
    }

    /// Finds the closing `>` that brings angle depth from `initial_depth` to 0, starting at token
    /// `start` up to `limit`, bailing on a hole or an unmatched `;`/`{`/`}`.
    fn closing_angle_from(&self, start: usize, initial_depth: usize, limit: usize) -> Option<usize> {
        let mut angle_depth = initial_depth;
        let mut group_depth = 0usize;
        let mut j = start;
        // Bounded: one turn per token up to `limit`.
        while j < limit {
            let t = self.tok(j)?;
            match t.k {
                K::Hole => return None,
                K::P(b';' | b'{' | b'}') if group_depth == 0 => return None,
                K::P(b'(' | b'[') => group_depth += 1,
                K::P(b')' | b']') => group_depth = group_depth.checked_sub(1)?,
                K::P(b'<') if group_depth == 0 => angle_depth += 1,
                K::P(b'>') if group_depth == 0 && !self.arrow(j) => {
                    angle_depth = angle_depth.checked_sub(1)?;
                    if angle_depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
            j += 1;
        }
        None
    }

    /// Flags literal `"sandbox-exec"` or `"/usr/bin/sandbox-exec"` outside `bstd::procs::hermetic`.
    fn raw_sandbox_exec(&mut self, t: Tok) {
        // Minimum length is `"sandbox-exec"` (14 bytes including quotes).
        if t.end - t.start < 14 {
            return;
        }
        let Some(inner) = unquote_string_literal(self.str_of(t)) else { return };
        if is_sandbox_exec_program(inner) {
            self.push(Template::RawSandboxExec, t.start, t.end, None);
        }
    }

    /// The `<..>` opening at `open` (to `limit`): its count of top-level arguments (a trailing
    /// comma adds none), its first and its last argument as token ranges.
    fn type_args(&self, open: usize, limit: usize) -> Option<TypeArgs> {
        let mut depth = 0usize;
        let mut count = 0usize;
        let mut from = open + 1;
        let mut first = None;
        let mut last = (from, from);
        let mut j = open;
        // Bounded: one turn per token up to `limit`.
        while j < limit {
            let t = self.tok(j)?;
            match t.k {
                K::P(b'<' | b'(' | b'[') => depth += 1,
                K::P(b'>') if self.arrow(j) => {}
                K::P(b'>' | b')' | b']') => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        if j > from {
                            count += 1;
                            last = (from, j);
                            first.get_or_insert(last);
                        }
                        return Some(TypeArgs { count, first: first.unwrap_or(last), last });
                    }
                }
                K::P(b',') if depth == 1 => {
                    count += 1;
                    last = (from, j);
                    first.get_or_insert(last);
                    from = j + 1;
                }
                _ => {}
            }
            j += 1;
        }
        None
    }

    /// allocs.rs's `fixes_allocator` over tokens `from..to`: the first type that fixes its
    /// caller's allocator.
    fn fixes_allocator(&self, from: usize, to: usize) -> Option<Span> {
        let mut k = from;
        // Bounded: one turn per token of the signature.
        while k < to {
            if let Some(word) = self.word(self.tok(k)) {
                let next_open = k + 1 < to && matches!(self.tok(k + 1), Some(n) if n.k == K::P(b'<'));
                let t = self.toks[k];
                if word == "String" && !next_open {
                    return Some(Span::new(t.start, t.end));
                }
                if let Some(&(_, arity)) = ALLOCATING.iter().find(|(name, _)| *name == word)
                    && next_open
                    && let Some(TypeArgs { count, last, .. }) = self.type_args(k + 1, to)
                    && (count + 1 == arity || (count == arity && self.names_global(last)))
                {
                    let end = self.closing_angle(k + 1, to).map_or(t.end, |c| self.toks[c].end);
                    return Some(Span::new(t.start, end));
                }
            }
            k += 1;
        }
        None
    }

    /// The argument `range` is `Global` or ends in `::Global`.
    fn names_global(&self, (from, to): (usize, usize)) -> bool {
        to > from && self.is_word(self.tok(to - 1), "Global") && (to - 1 == from || self.colons_before(to - 1))
    }

    fn closing_angle(&self, open: usize, limit: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut j = open;
        // Bounded: one turn per token up to `limit`.
        while j < limit {
            match self.tok(j)?.k {
                K::P(b'<' | b'(' | b'[') => depth += 1,
                K::P(b'>') if self.arrow(j) => {}
                K::P(b'>' | b')' | b']') => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
            j += 1;
        }
        None
    }

    /// untyped.rs's `untyped_errors` over tokens `from..to`: per kind (string, str, boxed,
    /// anyhow), the range of the first error of it.
    fn untyped_errors(&self, from: usize, to: usize) -> [Option<Span>; 4] {
        let mut found = [None; 4];
        let mut k = from;
        // Bounded: one turn per token of the signature.
        while k < to {
            match self.word(self.tok(k)) {
                Some("anyhow") if self.colons_at(k + 1) => {
                    let then = self.tok(k + 4);
                    let anyhow = match self.word(self.tok(k + 3)) {
                        Some("Error") => true,
                        Some("Result") => match then {
                            Some(n) if n.k == K::P(b'<') => {
                                self.type_args(k + 4, to).is_some_and(|args| args.count == 1)
                            }
                            _ => true,
                        },
                        _ => false,
                    };
                    if anyhow && found[3].is_none() {
                        let end = self.tok(k + 3).map_or(self.toks[k].end, |s| s.end);
                        found[3] = Some(Span::new(self.toks[k].start, end));
                    }
                }
                Some("Result") if matches!(self.tok(k + 1), Some(n) if n.k == K::P(b'<')) => {
                    if let Some(TypeArgs { count, last, .. }) = self.type_args(k + 1, to)
                        && count >= 2
                        && let Some(kind) = self.error_kind(last)
                        && found[kind].is_none()
                    {
                        found[kind] = Some(Span::new(self.toks[last.0].start, self.toks[last.1 - 1].end));
                    }
                }
                _ => {}
            }
            k += 1;
        }
        found
    }

    /// untyped.rs's `error_kind`: 0 `String`, 1 `&str`, 2 `Box<dyn ..Error..>`, 3 anyhow's.
    fn error_kind(&self, (from, to): (usize, usize)) -> Option<usize> {
        let first = self.tok(from)?;
        if first.k == K::P(b'&') {
            let mut k = from + 1;
            if matches!(self.tok(k), Some(t) if t.k == K::Life) {
                k += 1;
            }
            if self.is_word(self.tok(k), "mut") {
                k += 1;
            }
            return (k + 1 == to && self.is_word(self.tok(k), "str")).then_some(1);
        }
        let mut path_to = from;
        // Bounded: one turn per token of the argument.
        while path_to < to && !matches!(self.tok(path_to), Some(t) if t.k == K::P(b'<')) {
            path_to += 1;
        }
        let last = path_to.checked_sub(1).filter(|l| *l >= from)?;
        let segment = self.word(self.tok(last))?;
        if last != from && !self.colons_before(last) {
            return None;
        }
        match segment {
            "String" => Some(0),
            "Error" if (from..last).any(|k| self.is_word(self.tok(k), "anyhow")) => Some(3),
            "Box" if path_to < to => {
                let TypeArgs { first: (a, b), .. } = self.type_args(path_to, to)?;
                if !self.is_word(self.tok(a), "dyn") {
                    return None;
                }
                // Each `+` part's last segment before its `<`.
                let mut part_end = a + 1;
                let mut k = a + 1;
                // Bounded: one turn per token of the first argument.
                while k <= b {
                    let at_end = k == b || matches!(self.tok(k), Some(t) if t.k == K::P(b'+'));
                    if at_end {
                        let mut seg_end = part_end;
                        // Bounded: one turn per token of the part.
                        while seg_end < k && !matches!(self.tok(seg_end), Some(t) if t.k == K::P(b'<')) {
                            seg_end += 1;
                        }
                        if seg_end > part_end && self.is_word(self.tok(seg_end - 1), "Error") {
                            return Some(2);
                        }
                        part_end = k + 1;
                    }
                    k += 1;
                }
                None
            }
            _ => None,
        }
    }

    /// untyped.rs's `alias_eq`: the first `=` outside brackets, not `==` or `=>`.
    fn alias_eq(&self, from: usize, to: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut k = from;
        // Bounded: one turn per token of the item.
        while k < to {
            let t = self.tok(k)?;
            match t.k {
                K::P(b'<' | b'(' | b'[' | b'{') => depth += 1,
                K::P(b'>') if self.arrow(k) => {}
                K::P(b'>' | b')' | b']' | b'}') => depth = depth.checked_sub(1)?,
                K::P(b'=') if depth == 0 => {
                    let joined = matches!(self.tok(k + 1), Some(n) if matches!(n.k, K::P(b'=' | b'>')) && n.start == t.end);
                    if !joined {
                        return Some(k);
                    }
                }
                _ => {}
            }
            k += 1;
        }
        None
    }

    /// recursion.rs: in the innermost open body, its function's name called as the function
    /// is (bare or `self::` for a free one, `Self::` or `self.` for an associated one).
    fn recursion_name(&mut self, i: usize, t: Tok) {
        let Some(&inner) = self.bodies.last() else { return };
        let f = self.fns[inner as usize];
        if f.recursive || f.name.len() != t.end - t.start || f.name.of(self.text) != self.str_of(t) {
            return;
        }
        let recursive = match self.call_of(i) {
            Some(Called::Bare | Called::SelfModule) => !f.assoc,
            Some(Called::SelfType) => f.assoc,
            Some(Called::SelfMethod) => f.assoc && f.has_self,
            Some(Called::Other) | None => false,
        };
        if recursive {
            self.fns[inner as usize].recursive = true;
            if !f.marked {
                self.push(Template::Recursion, f.name.start, f.name.end, Some(Arg::Span(Span::new(t.start, t.end))));
            }
        }
    }

    /// recursion.rs's `call_of`.
    fn call_of(&self, i: usize) -> Option<Called> {
        let mut j = i + 1;
        if self.colons_at(j) {
            if !matches!(self.tok(j + 2), Some(t) if t.k == K::P(b'<')) {
                return None;
            }
            let mut depth = 0usize;
            let mut k = j + 2;
            // Bounded: one turn per token to the turbofish's `>`.
            loop {
                match self.tok(k)?.k {
                    K::P(b'<') => depth += 1,
                    K::P(b'>') => depth = depth.saturating_sub(1),
                    _ => {}
                }
                if depth == 0 {
                    break;
                }
                k += 1;
            }
            j = k + 1;
        }
        if !matches!(self.tok(j), Some(t) if t.k == K::P(b'(')) {
            return None;
        }
        let prev = self.back(i, 1);
        match prev {
            Some(p) if p.k == K::P(b'.') => {
                if matches!(self.back(i, 2), Some(t) if t.k == K::P(b'.') && t.end == p.start) {
                    return Some(Called::Other);
                }
                let receiver_self = self.is_word(self.back(i, 2), "self");
                let chained = matches!(self.back(i, 3), Some(t) if t.k == K::P(b'.'));
                Some(if receiver_self && !chained { Called::SelfMethod } else { Called::Other })
            }
            _ if self.colons_before(i) => {
                let longer = self.colons_before(i - 3);
                Some(match self.word(self.back(i, 3)) {
                    Some("Self") if !longer => Called::SelfType,
                    Some("self") if !longer => Called::SelfModule,
                    _ => Called::Other,
                })
            }
            Some(p) if self.is_word(Some(p), "fn") || p.k == K::P(b'$') => None,
            _ => Some(Called::Bare),
        }
    }

    // ---- stops --------------------------------------------------------------------------

    /// stops.rs: `.unwrap()` and `.expect(`. Its macros (`panic!`, `unreachable!`, `todo!`,
    /// `unimplemented!`, not after `.`, not `!=`) are sequences (`sequences::SEQS`).
    fn stop(&mut self, i: usize, t: Tok, word_id: u8) {
        let next = self.tok(i + 1);
        let method = self.method(i);
        match word_id {
            W_UNWRAP if method && matches!(next, Some(n) if n.k == K::P(b'(')) => {
                if let Some(close) = self.tok(i + 2)
                    && close.k == K::P(b')')
                {
                    self.push(Template::StopUnwrap, t.start, close.end, None);
                }
            }
            W_EXPECT if method && matches!(next, Some(n) if n.k == K::P(b'(')) => {
                self.push(Template::StopExpect, t.start, t.end, None);
            }
            _ => {}
        }
    }

    /// stops.rs's `is_index`: a `[` after an expression whose contents are not empty, hold no
    /// `;`, and are not an integer literal or a range of them.
    fn index(&mut self, i: usize) {
        let Some(prev) = self.back(i, 1) else { return };
        let after_expression = match prev.k {
            K::Ident => !NOT_BEFORE_INDEX.contains(&self.str_of(prev)),
            K::Raw => !NOT_BEFORE_INDEX.contains(&&self.str_of(prev)[2..]),
            K::Int | K::Float => true,
            K::P(b')' | b']' | b'?') => true,
            _ => false,
        };
        if !after_expression {
            return;
        }
        let Some(close) = self.closing(i) else { return };
        if close == i + 1 || (i + 1..close).any(|k| matches!(self.tok(k), Some(t) if t.k == K::P(b';'))) {
            return;
        }
        if self.literal_or_range(i + 1, close) {
            return;
        }
        let (open, end) = (self.toks[i], self.toks[close]);
        self.push(Template::StopIndex, open.start, end.end, None);
    }

    fn literal_or_range(&self, from: usize, to: usize) -> bool {
        let literal = |a: usize, b: usize| b == a || (b == a + 1 && matches!(self.tok(a), Some(t) if t.k == K::Int));
        let mut k = from;
        // Bounded: one turn per token inside the brackets.
        while k + 1 < to {
            let (a, b) = (self.toks[k], self.toks[k + 1]);
            if a.k == K::P(b'.') && b.k == K::P(b'.') && a.end == b.start {
                let mut rest = k + 2;
                if matches!(self.tok(rest), Some(t) if t.k == K::P(b'=') && t.start == b.end) && rest < to {
                    rest += 1;
                }
                return literal(from, k) && literal(rest, to);
            }
            k += 1;
        }
        to > from && literal(from, to)
    }

    // ---- ambient ------------------------------------------------------------------------

    /// ambient.rs: `std` or `alloc` as a path's first segment, followed by `::` or after the
    /// word `use` or `crate`.
    fn ambient_head(&mut self, i: usize, t: Tok, word_id: u8) {
        let field = self.method(i);
        let segment_before = self.colons_before(i) && self.word(self.back(i, 3)).is_some();
        if field || segment_before {
            return;
        }
        let path = self.colons_at(i + 1);
        let after_word = self.is_word(self.back(i, 1), "use") || self.is_word(self.back(i, 1), "crate");
        if !path && !after_word {
            return;
        }
        let template = if word_id == W_STD { Template::AmbientStd } else { Template::AmbientAlloc };
        let arg = if path {
            self.tok(i + 3).filter(|s| s.k == K::Ident).map(|s| Arg::Span(Span::new(s.start, s.end)))
        } else {
            None
        };
        self.push(template, t.start, t.end, arg);
    }

    /// ambient.rs's `vec_box` for a collection named through `std::` or `alloc::` (the bare
    /// names wait for the file's imports: a rule of the second kind, not built).
    fn collection(&mut self, i: usize, t: Tok, word_id: u8) {
        let shaped =
            word_id == W_STRING || matches!(self.tok(i + 1), Some(n) if n.k == K::P(b'<')) || self.colons_at(i + 1);
        if !shaped || self.method(i) {
            return;
        }
        let mut head = i;
        // Bounded: each turn moves `head` back by one segment.
        while self.colons_before(head) && self.word(self.back(head, 3)).is_some() {
            head -= 3;
        }
        if head == i {
            return;
        }
        let first = self.toks[head];
        if first.word == W_STD || first.word == W_ALLOC {
            self.push(Template::AmbientCollection, first.start, t.end, Some(Arg::Span(Span::new(first.start, first.end))));
        }
    }

    // ---- process statics and sans-IO cores -------------------------------------------------

    /// `thread_local!`, `static mut`, or `static NAME: T` where a word of `T` names interior
    /// state ([`stateful`]): D170 C3's state the process keeps. A static of
    /// [`crate::ALLOWED_STATICS`] in its crate (the scope's `statics` rows, by name) is one of
    /// C3's three and not reported; a `thread_local!` is not when every `static` it declares is
    /// such a row. Text cannot see a stateful type behind a name of the crate's own
    /// (`static X: Table`), so the rule says less, never more.
    fn process_static(&mut self, i: usize, t: Tok, word_id: u8) {
        let next = self.tok(i + 1);
        if word_id == W_THREAD_LOCAL {
            if let Some(bang) = next
                && bang.k == K::P(b'!')
                && !self.method(i)
                && !self.thread_local_allowed(i + 2)
            {
                self.push(Template::StaticThreadLocal, t.start, bang.end, None);
            }
            return;
        }
        // `static` the word (`'static` is a lifetime token, never this word).
        let named = if self.is_word(next, "mut") { self.word(self.tok(i + 2)) } else { self.word(next) };
        if named.is_some_and(|name| crate::allowed::allows(self.scope.statics, name)) {
            return;
        }
        if let Some(m) = next
            && self.is_word(next, "mut")
        {
            self.push(Template::StaticMut, t.start, m.end, None);
            return;
        }
        if self.word(next).is_none() || !matches!(self.tok(i + 2), Some(c) if c.k == K::P(b':')) {
            return;
        }
        let mut depth = 0usize;
        let mut j = i + 3;
        // Bounded: each turn moves `j` forward, to the item's last token at most.
        while let Some(tok) = self.tok(j) {
            match tok.k {
                K::P(b'<' | b'(' | b'[') => depth += 1,
                K::P(b'>' | b')' | b']') => depth = depth.saturating_sub(1),
                K::P(b'=' | b';') if depth == 0 => return,
                K::Ident if stateful(self.str_of(tok)) => {
                    self.push(Template::StaticState, t.start, tok.end, Some(Arg::Span(Span::new(tok.start, tok.end))));
                    return;
                }
                _ => {}
            }
            j += 1;
        }
    }

    /// The `thread_local!` whose bracket is at token `open` declares one `static` or more, and
    /// each is an allowed static of the scope's crate. A body that does not close in this item
    /// is not allowed.
    fn thread_local_allowed(&self, open: usize) -> bool {
        let Some(close) = self.closing(open) else { return false };
        let mut any = false;
        // Bounded: one turn per token of the macro's body.
        for k in open + 1..close {
            if self.is_word(self.tok(k), "static") {
                let name = if self.is_word(self.tok(k + 1), "mut") { self.word(self.tok(k + 2)) } else { self.word(self.tok(k + 1)) };
                match name {
                    Some(name) if crate::allowed::allows(self.scope.statics, name) => any = true,
                    _ => return false,
                }
            }
        }
        any
    }

    /// In a sans-IO core, a word of [`SANS_IO_WORDS`]: a path through an IO module of std
    /// (`std::fs`, or `fs::` as a path's head), an IO type or system-call crate, `File::open`
    /// and its kin, a blocking call (`.recv()`, `.join()`, `.lock()` with no argument, `.wait(`,
    /// `block_on(`), the wall clock (`Instant::now`, `SystemTime::now`), a print macro. A bare head
    /// `io::` is not reported in a file that declares or imports `io` as its own module
    /// ([`crate::OwnHeads`], in the scope): there the name cannot be std's.
    fn sans_io_word(&mut self, i: usize, t: Tok, word_id: u8, class: u16) {
        let next = self.tok(i + 1);
        let open_next = matches!(next, Some(n) if n.k == K::P(b'('));
        let (template, end) = if class & WC_IO_MODULE != 0 {
            if self.method(i) {
                return;
            }
            let w = self.str_of(t);
            let after_std = self.colons_before(i) && matches!(self.back(i, 3), Some(p) if p.word == W_STD);
            // A bare head is std's only when the file does not make that name its own
            // (`mod io;`, `use crate::io;`: the scope's `own_heads`, read by `OwnHeads::read`).
            let own = IO_MODULES.iter().position(|m| *m == w).is_some_and(|at| self.scope.own_heads.0 & (1 << at) != 0);
            let head = !self.colons_before(i) && self.colons_at(i + 1) && !own;
            if !after_std && !head {
                return;
            }
            (Template::SansIoPath, t.end)
        } else if class & WC_IO_TYPE != 0 {
            let crate_path = word_id == W_LIBC || word_id == W_RUSTIX;
            if self.method(i) || (crate_path && !self.colons_at(i + 1)) {
                return;
            }
            (Template::SansIoPath, t.end)
        } else if word_id == W_FILE {
            let call = self.tok(i + 3).filter(|c| self.colons_at(i + 1) && FILE_CALLS.contains(&self.str_of(*c)));
            match call {
                Some(call) => (Template::SansIoPath, call.end),
                None => return,
            }
        } else if matches!(word_id, W_RECV | W_JOIN | W_LOCK) {
            let no_arg = matches!(self.tok(i + 2), Some(c) if c.k == K::P(b')'));
            if !self.method(i) || !open_next || !no_arg {
                return;
            }
            (Template::SansIoBlocking, t.end)
        } else if word_id == W_WAIT {
            if !self.method(i) || !open_next {
                return;
            }
            (Template::SansIoBlocking, t.end)
        } else if word_id == W_BLOCK_ON {
            if !open_next || matches!(self.back(i, 1), Some(p) if p.word == W_FN) {
                return;
            }
            (Template::SansIoBlocking, t.end)
        } else if matches!(word_id, W_INSTANT | W_SYSTEM_TIME) {
            let now = self.tok(i + 3).filter(|n| self.colons_at(i + 1) && self.str_of(*n) == "now");
            match now {
                Some(now) => (Template::SansIoClock, now.end),
                None => return,
            }
        } else if class & WC_PRINT_MACRO != 0 {
            match next {
                Some(bang) if bang.k == K::P(b'!') && !self.method(i) => (Template::SansIoPrint, bang.end),
                _ => return,
            }
        } else {
            return;
        };
        self.push(template, t.start, end, None);
    }

    // ---- locks (D179) -------------------------------------------------------------------

    /// The `#` at token `i` starts `#[blaze::lock_exception(..)]`: what follows it, to
    /// [`Pass::reason_end`], has its reason. One whose arguments are not `tried` with one of
    /// D179's three shapes and a `why` that says something, each once and nothing else, is
    /// refused naming the attribute; it still covers what follows, so one mistake is one finding.
    fn lock_exception(&mut self, i: usize, t: Tok) -> bool {
        let named = matches!(self.tok(i + 1), Some(b) if b.k == K::P(b'['))
            && self.is_word(self.tok(i + 2), "blaze")
            && self.colons_at(i + 3)
            && self.is_word(self.tok(i + 5), "lock_exception");
        if !named {
            return false;
        }
        let open = i + 6;
        let args = match self.tok(open) {
            Some(p) if p.k == K::P(b'(') => self.closing(open),
            _ => None,
        };
        let close = match args {
            Some(close) => close,
            None => i + 5,
        };
        let Some(bracket) = self.tok(close + 1).filter(|b| b.k == K::P(b']')) else {
            // Not an attribute this reads to its end: the whole of it is the finding, and it
            // covers nothing.
            let end = self.tok(close).map_or(t.end, |c| c.end);
            self.push(Template::LockBadReason, t.start, end, None);
            return true;
        };
        if args.is_none() || !self.lock_reason_well_formed(open, close) {
            self.push(Template::LockBadReason, t.start, bracket.end, None);
        }
        let end = self.reason_end(close + 2);
        self.reasons.push(Span::new(t.start, end));
        true
    }

    /// The tokens strictly between `open` and `close` are `tried = "<shape>", why = "<text>"`,
    /// in either order, a trailing comma allowed: each key once, `tried` one of
    /// [`LOCK_SHAPES`], `why` a string literal with something in it.
    fn lock_reason_well_formed(&self, open: usize, close: usize) -> bool {
        let (mut tried, mut why) = (false, false);
        let mut j = open + 1;
        // Bounded: each turn moves `j` forward by three or four tokens, to `close` at most.
        while j < close {
            let (Some(key), Some(eq), Some(value)) = (self.word(self.tok(j)), self.tok(j + 1), self.tok(j + 2)) else {
                return false;
            };
            if eq.k != K::P(b'=') || value.k != K::Lit || j + 2 >= close {
                return false;
            }
            let text = self.str_of(value);
            match key {
                "tried" if !tried && LOCK_SHAPES.contains(&text) => tried = true,
                "why" if !why && says_something(text) => why = true,
                _ => return false,
            }
            j += 3;
            match self.tok(j) {
                Some(c) if j < close && c.k == K::P(b',') => j += 1,
                _ if j == close => {}
                _ => return false,
            }
        }
        tried && why
    }

    /// The end of what a `lock_exception` whose `]` ends just before token `from` covers: the
    /// first `;` or `,` outside brackets (a statement, a `static` or `const`, a field and its
    /// value), a child item (its hole), the `}` that closes the first brace unless `else`
    /// follows it, or a closing bracket of an enclosing expression; the item's end when none.
    fn reason_end(&self, from: usize) -> u32 {
        let mut depth = 0usize;
        let mut j = from;
        // Bounded: each turn moves `j` forward, to the last token at most.
        while let Some(t) = self.tok(j) {
            match t.k {
                K::Hole if depth == 0 => return t.end,
                K::P(b'{' | b'(' | b'[') => depth += 1,
                K::P(c @ (b'}' | b')' | b']')) => {
                    if depth == 0 {
                        return t.start;
                    }
                    depth -= 1;
                    if c == b'}' && depth == 0 && !self.is_word(self.tok(j + 1), "else") {
                        return t.end;
                    }
                }
                K::P(b';' | b',') if depth == 0 => return t.end,
                _ => {}
            }
            j += 1;
        }
        // `run` refused a text longer than `u32::MAX` bytes, so this is its length.
        u32::try_from(self.text.len()).unwrap_or(u32::MAX)
    }

    /// Byte `pos` is in what a `lock_exception` of this item covers.
    fn reasoned(&self, pos: u32) -> bool {
        self.reasons.iter().any(|span| span.start <= pos && pos < span.end)
    }

    /// A word of [`LOCK_TYPES`]: `Type::ctor` (a turbofish between allowed, `Lock::<T>::new`)
    /// naming one of D179's constructors, called or named as a function, outside a `use`. With
    /// no reason over it, here or on an ancestor item ([`crate::Scope::lock_reason`]), it is
    /// one finding over the path. `Self::new` inside the type's own impl is not seen (the rule
    /// says less, never more).
    fn lock_ctor(&mut self, i: usize, t: Tok, w: &str) {
        if self.in_use || self.method(i) || (self.colons_before(i) && !self.lock_path_head(i)) || !self.colons_at(i + 1) {
            return;
        }
        let mut at = i + 3;
        if matches!(self.tok(at), Some(lt) if lt.k == K::P(b'<')) {
            let Some(gt) = self.closing_angle(at, self.toks.len()) else { return };
            if !self.colons_at(gt + 1) {
                return;
            }
            at = gt + 3;
        }
        let Some(ctor) = self.tok(at).filter(|c| c.k == K::Ident) else { return };
        let name = self.str_of(ctor);
        let constructs = match w {
            "Lock" => matches!(name, "new" | "unrecorded" | "probed"),
            "ReadLock" => matches!(name, "new" | "unrecorded"),
            "RwLock" | "RankedLock" => name == "new",
            "SharedMap" => name == "new_in",
            _ => false,
        };
        if !constructs || self.scope.lock_reason || self.reasoned(t.start) {
            return;
        }
        // The lock's name, when its first argument is a plain string literal (lock-policy.md):
        // the text between its quotes, which the message says in place of the path.
        let name = match (self.tok(at + 1), self.tok(at + 2), self.tok(at + 3)) {
            (Some(open), Some(lit), Some(after))
                if open.k == K::P(b'(')
                    && lit.k == K::Lit
                    && matches!(after.k, K::P(b',' | b')'))
                    && self.str_of(lit).len() >= 2
                    && self.str_of(lit).starts_with('"') =>
            {
                Some(Arg::Span(Span::new(lit.start + 1, lit.end - 1)))
            }
            _ => None,
        };
        self.push(Template::LockNoReason, t.start, ctor.end, name);
    }

    /// The `::` before token `i` follows a path whose words are lowercase modules
    /// (`bstd::sync::Lock`, `blaze_core::lock::Lock`): the type is still the path's last type.
    /// A type's associated item (`Outer::Lock::new`) is not this.
    fn lock_path_head(&self, i: usize) -> bool {
        self.word(self.back(i, 3)).is_some_and(|m| m.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b == b'_'))
    }

    // ---- unsafe -------------------------------------------------------------------------

    /// An `unsafe` block or impl with no `// SAFETY:` comment on its line or above it (the
    /// clones walk: comment lines and a statement's unended lines are passed over).
    fn unsafe_word(&mut self, i: usize, t: Tok) {
        let next = self.tok(i + 1);
        let opens = matches!(next, Some(n) if n.k == K::P(b'{') || n.word == W_IMPL);
        if !opens {
            return;
        }
        let context = self.scope.context;
        let after = context.safety_after.then_some(Mark::Safety);
        if self.look(t.start as usize, safety_mark, context.safety_above, after).is_none() {
            self.push(Template::UnsafeNoComment, t.start, t.end, None);
        }
    }

    /// `unsafe {`, `unsafe fn`, `unsafe impl`, `unsafe trait`, `unsafe extern`, and
    /// `#[unsafe(..)]` / `#![unsafe(..)]` when the crate does not carry a valid
    /// `#![blaze::trust(unsafe, reason = "..")]` grant.
    fn workspace_unsafe_word(&mut self, i: usize, t: Tok) {
        let next = self.tok(i + 1);
        let is_item_or_block = match next {
            Some(n) if n.k == K::P(b'{') => true,
            Some(n) if n.k == K::Ident => matches!(n.word, W_FN | W_IMPL | W_TRAIT | W_EXTERN),
            _ => false,
        };
        let is_attr = matches!(next, Some(n) if n.k == K::P(b'('))
            && matches!(self.back(i, 1), Some(b) if b.k == K::P(b'['))
            && (matches!(self.back(i, 2), Some(p) if p.k == K::P(b'#'))
                || (matches!(self.back(i, 2), Some(b) if b.k == K::P(b'!'))
                    && matches!(self.back(i, 3), Some(p) if p.k == K::P(b'#'))));
        if is_item_or_block || is_attr {
            self.push(Template::WorkspaceUnsafe, t.start, t.end, None);
        }
    }

    /// `Ordering::SeqCst` without a `// seqcst: <why>` comment on its line or above it.
    fn seqcst_word(&mut self, i: usize, t: Tok) {
        if self.in_use || self.method(i) || !self.colons_at(i + 1) {
            return;
        }
        let Some(variant) = self.tok(i + 3) else { return };
        if variant.k != K::Ident || variant.word != W_SEQCST {
            return;
        }
        let context = self.scope.context;
        let after = context.seqcst_after.then_some(Mark::SeqCst);
        if self.look(t.start as usize, seqcst_mark, context.seqcst_above, after).is_none() {
            self.push(Template::SeqCstUnexplained, t.start, variant.end, None);
        }
    }

    // ---- the sequences ------------------------------------------------------------------

    /// The symbol of token `i` for the matcher, and how many tokens it takes: `::`, `..` and
    /// `!=` are one symbol of two adjacent tokens.
    #[inline(always)]
    fn symbol(&self, i: usize, t: Tok) -> (u8, usize) {
        let joined = |c: u8| matches!(self.tok(i + 1), Some(n) if n.k == K::P(c) && n.start == t.end);
        match t.k {
            K::Ident => {
                if t.word != NO_WORD {
                    (t.word, 1)
                } else {
                    (OTHER, 1)
                }
            }
            K::P(b':') if joined(b':') => (SEP, 2),
            K::P(b'.') if joined(b'.') => (DOTDOT, 2),
            K::P(b'!') if joined(b'=') => (NE, 2),
            K::P(b'.') => (DOT, 1),
            K::P(b'(') => (OPEN, 1),
            K::P(b')') => (CLOSE, 1),
            K::P(b'!') => (BANG, 1),
            K::P(b'{') => (BRACE, 1),
            K::P(b'|') => (BAR, 1),
            K::P(b'<') => (LT, 1),
            // A hole is here too: no sequence matches across a child item.
            _ => (OTHER, 1),
        }
    }

    /// Every sequence of `sequences::SEQS` found in one walk over the tokens: the live trie
    /// states advanced by each token's symbol, each sequence that ends handed to
    /// [`Pass::on_sequence`] with its first and last token.
    ///
    /// One finding per site where two rules' sequences overlap on one table: a std or hashbrown
    /// path (`collections::HashMap`, `hash_map::RandomState`, `ambient-hasher`'s `std-table`)
    /// is the more specific finding, so a constructor sequence that starts at its last token
    /// (`HashMap::new(`, `RandomState::default(` of `ambient-read`; `HashMap::new_in(` of
    /// `ambient-hasher`) is not pushed after it. Where `ambient-hasher` does not run, the path
    /// is no finding and the constructor's is.
    fn sequences(&mut self) {
        let n = self.toks.len();
        let mut live = Live::new();
        // The last token of the latest `std-table` finding pushed; none yet.
        let mut std_table_at = usize::MAX;
        let mut i = 0;
        // Bounded: each turn moves `i` forward by one or two tokens.
        while i < n {
            let t = self.toks[i];
            // Fast path when no sequence match is live: skip tokens that can never start one
            // without running `symbol()` or touching `TRIE`.
            if live.is_empty() {
                match t.k {
                    K::Ident => {
                        if t.word == NO_WORD {
                            i += 1;
                            continue;
                        }
                        let to = TRIE.root(t.word);
                        if to == DEAD {
                            i += 1;
                            continue;
                        }
                        live.start_root(to, i as u32);
                        let last = i;
                        i += 1;
                        let mut seq = TRIE.first_end(to);
                        while seq != NO_SEQ {
                            let template = SEQS[seq as usize].template;
                            let covered = last == std_table_at
                                && matches!(template, Template::ReadHashSeed | Template::HasherDefault);
                            if !covered && self.on_sequence(seq as usize, last, last) && template == Template::HasherStd {
                                std_table_at = last;
                            }
                            seq = TRIE.next_end(seq);
                        }
                        continue;
                    }
                    // Only `.` (for `.await`, `.to_string()`, `.recv()`, etc.) and `:`/`!`/`(`/`{`/`|`/`<`
                    // can ever map to non-OTHER punctuation; skip everything else in 1 branch.
                    K::P(b'.' | b':' | b'!' | b'(' | b')' | b'{' | b'|' | b'<') => {}
                    _ => {
                        i += 1;
                        continue;
                    }
                }
            }
            let (sym, width) = self.symbol(i, t);
            let tok_start = i as u32;
            let last = i + width - 1;
            i += width;
            // `OTHER` has no transition from any node: clear live states without touching `TRIE`.
            if sym == OTHER {
                live.clear();
                continue;
            }
            if live.is_empty() {
                let to = TRIE.root(sym);
                if to == DEAD {
                    continue;
                }
                live.start_root(to, tok_start);
            } else {
                live.step(&TRIE, sym, tok_start);
            }
            // Bounded: one turn per live state (the longest sequence's length at most), and
            // within it one per sequence that ends at its node.
            for at in 0..live.len() {
                let (node, start) = live.state(at);
                let mut seq = TRIE.first_end(node);
                while seq != NO_SEQ {
                    let template = SEQS[seq as usize].template;
                    let covered = start as usize == std_table_at
                        && matches!(template, Template::ReadHashSeed | Template::HasherDefault);
                    if !covered && self.on_sequence(seq as usize, start as usize, last) && template == Template::HasherStd {
                        std_table_at = last;
                    }
                    seq = TRIE.next_end(seq);
                }
            }
        }
    }

    /// Sequence `row` matched from token `first` to token `last`: its guard read, its finding
    /// pushed when its rule runs. Whether it was pushed.
    fn on_sequence(&mut self, row: usize, first: usize, last: usize) -> bool {
        let seq = &SEQS[row];
        if !self.rules.runs(seq.template.rule()) {
            return false;
        }
        self.cost.add(seq.template.rule());
        let pass = match seq.guard {
            Guard::None => true,
            Guard::NotMethod => !self.method(first) && !self.is_word(self.back(first, 1), "fn"),
            Guard::Unbounded => self.unbounded(last),
        };
        if !pass {
            return false;
        }
        let (from, to) = match seq.cut {
            Cut::Whole => (first, last),
            Cut::DropLast => (first, last - 1),
            Cut::DropFirst => (first + 1, last),
            Cut::Inner => (first + 1, last - 1),
            Cut::DropTurbo => (first, last - 3),
        };
        let (a, b) = (self.toks[from], self.toks[to]);
        let arg = match seq.arg {
            ArgOf::None => None,
            ArgOf::Row => Some(Arg::Int(row as u32)),
            ArgOf::FirstToken => {
                let head = self.toks[first];
                Some(Arg::Span(Span::new(head.start, head.end)))
            }
        };
        let plan = match seq.template {
            Template::UndoneDbg => self.dbg_fix(first),
            _ => Plan::None,
        };
        self.cands.push(Cand { template: seq.template, range: Span::new(a.start, b.end), arg, plan });
        true
    }

    /// `loop {` with its `{` at `brace`: whether nothing between the braces leaves it (no
    /// `break`, `return` or `?`; a child item, a hole, is not read). A `break` of an inner loop
    /// or a `return` in a closure counts too: text cannot tell them apart, so the rule says less,
    /// never more. A brace that never closes in this item reports nothing.
    fn unbounded(&self, brace: usize) -> bool {
        let Some(close) = self.closing(brace) else { return false };
        // Bounded: one turn per token of the loop's body.
        !(brace + 1..close).any(|k| {
            let t = self.toks[k];
            t.k == K::P(b'?') || (t.k == K::Ident && matches!(self.str_of(t), "break" | "return"))
        })
    }

    /// The fix of `dbg!(..)` whose `dbg` is token `at`: the four tokens `dbg`, `!`, `(`, `)`
    /// deleted, which leaves the value and drops the print. Only where the argument is a value
    /// whose meaning does not change without the macro around it ([`Pass::is_value`]): a call or
    /// a literal. `dbg!(x)` of a place moves `x` where `x` alone may borrow it, and an operator
    /// inside would bind to its neighbours: those are left for a person.
    fn dbg_fix(&self, at: usize) -> Plan {
        let (Some(name), Some(bang), Some(open)) = (self.tok(at), self.tok(at + 1), self.tok(at + 2)) else {
            return Plan::None;
        };
        if open.k != K::P(b'(') {
            return Plan::None;
        }
        let Some(close) = self.closing(at + 2) else { return Plan::None };
        if !self.is_value(at + 3, close) {
            return Plan::None;
        }
        let shut = self.toks[close];
        let span = |t: Tok| Span::new(t.start, t.end);
        Plan::Strip { tokens: [span(name), span(bang), span(open), span(shut)] }
    }

    /// Tokens `from..to` are one value expression that keeps its meaning without brackets
    /// around it: a literal alone, or a path or method chain (words, `.`, `::`) whose last part
    /// is a call's or a macro's group, or a `?` or `.await` after one. An operator, a `<`, a
    /// brace, a comma, a reference or a bare place (a name, a field, an index) is not one.
    fn is_value(&self, from: usize, to: usize) -> bool {
        if to == from + 1 {
            return matches!(self.tok(from), Some(t) if matches!(t.k, K::Int | K::Float | K::Lit));
        }
        let mut value = false;
        let mut k = from;
        // Bounded: each turn moves `k` forward, past a whole group at a bracket.
        while k < to {
            let Some(t) = self.tok(k) else { return false };
            let before = self.back(k, 1).filter(|_| k > from);
            match t.k {
                K::Ident => {
                    let w = self.str_of(t);
                    if matches!(
                        w,
                        "as" | "move" | "if" | "match" | "loop" | "while" | "for" | "unsafe" | "return" | "break"
                            | "let" | "async" | "ref" | "mut" | "dyn" | "impl" | "box"
                    ) {
                        return false;
                    }
                    value = w == "await" && matches!(before, Some(b) if b.k == K::P(b'.'));
                }
                K::Raw => value = false,
                K::Int | K::Float | K::Lit if k == from => value = true,
                K::P(b'.') => value = false,
                K::P(b':') if self.colons_at(k) || self.colons_before(k + 1) => value = false,
                K::P(b'!') if matches!(before, Some(b) if b.k == K::Ident && b.end == t.start) => value = false,
                K::P(b'(' | b'[') => {
                    let call = matches!(before, Some(b) if matches!(b.k, K::Ident | K::Raw | K::P(b')' | b'!')));
                    let macro_group = matches!(before, Some(b) if b.k == K::P(b'!'));
                    let Some(shut) = self.closing(k) else { return false };
                    if shut >= to {
                        return false;
                    }
                    value = (t.k == K::P(b'(') && call) || macro_group;
                    k = shut;
                }
                K::P(b'?') => value = true,
                _ => return false,
            }
            k += 1;
        }
        value
    }

    // ---- renamed ------------------------------------------------------------------------

    /// An old name of blaze-own's collections. Fixed where the receiver is known to be
    /// blaze-own's: a local whose every binding in this item is a typed `let` or parameter of a
    /// blaze-own type, or a `let` from a blaze-own constructor; or a path call through a
    /// blaze-own type (`AVec::insert(..)`). Reported for a person, not fixed, where the name is
    /// one only blaze-own has (`insert_many`, `try_insert_many`) and the receiver is unknown.
    /// Not reported otherwise: `insert`, `remove`, `contains` and `position` are also std's
    /// maps', sets', slices' and iterators'.
    fn renamed(&mut self, i: usize, t: Tok, w: &str) {
        let Some(open) = self.tok(i + 1) else { return };
        if open.k != K::P(b'(') {
            return;
        }
        let owner = if self.method(i) {
            match self.word(self.back(i, 2)) {
                Some(name) if !matches!(self.back(i, 3), Some(b) if b.k == K::P(b'.')) => {
                    // The bindings of the innermost open function's body before the call: the
                    // same tokens whether the function is linted alone or in its file.
                    let body = self.bodies.last().map(|f| self.fns[*f as usize].at as usize);
                    body.and_then(|from| self.owner_of_local(name, from, i))
                }
                _ => None,
            }
        } else if self.colons_before(i) {
            i.checked_sub(3).and_then(|k| self.owner_of_path_end(k))
        } else {
            return;
        };
        let renames = self.rules.renames();
        let row = owner.and_then(|o| renames.iter().position(|r| r.owner == o && r.old == w));
        match row {
            Some(row) => {
                let rename = renames[row];
                let after = if rename.suffix.is_empty() {
                    u32::MAX
                } else {
                    match self.closing(i + 1) {
                        Some(close) => self.toks[close].end,
                        None => return,
                    }
                };
                self.cands.push(Cand {
                    template: Template::RenamedFixed,
                    range: Span::new(t.start, t.end),
                    arg: Some(Arg::Int(row as u32)),
                    plan: Plan::Rename { row: row as u32, name: Span::new(t.start, t.end), after },
                });
            }
            None if ONLY_OWN.contains(&w) => {
                if let Some(row) = renames.iter().position(|r| r.owner == Owner::Vec && r.old == w) {
                    self.push(Template::RenamedReport, t.start, t.end, Some(Arg::Int(row as u32)));
                }
            }
            None => {}
        }
    }

    /// The owner every binding of `name` in tokens `from..to` agrees on, if one says one and
    /// none says otherwise or is a pattern this cannot read.
    fn owner_of_local(&self, name: &str, from: usize, to: usize) -> Option<Owner> {
        let mut agreed: Option<Owner> = None;
        let mut k = from;
        // Bounded: one turn per token of the range.
        while k < to {
            if self.is_word(self.tok(k), name) {
                let prev = self.back(k, 1);
                let prev2 = self.back(k, 2);
                let next = self.tok(k + 1);
                let typed = matches!(next, Some(c) if c.k == K::P(b':')) && !self.colons_at(k + 1);
                let after_let = self.is_word(prev, "let") || (self.is_word(prev, "mut") && self.is_word(prev2, "let"));
                let param_open = |t: Option<Tok>| matches!(t, Some(p) if matches!(p.k, K::P(b'(' | b',')));
                let param = typed && (param_open(prev) || (self.is_word(prev, "mut") && param_open(prev2)));
                let closure = matches!(prev, Some(p) if p.k == K::P(b'|')) || matches!(next, Some(c) if c.k == K::P(b'|'));
                let bound = self.is_word(prev, "for")
                    || matches!(next, Some(c) if c.k == K::P(b'@'))
                    || (matches!(prev, Some(p) if p.k == K::P(b'('))
                        && matches!(next, Some(c) if c.k == K::P(b')'))
                        && self.word(prev2).is_some_and(|w| w.starts_with(|c: char| c.is_ascii_uppercase())));
                let says = if after_let {
                    if typed {
                        Some(self.owner_of_type(k + 2))
                    } else if matches!(next, Some(c) if c.k == K::P(b'=')) {
                        Some(self.owner_of_init(k + 2))
                    } else {
                        Some(None)
                    }
                } else if param {
                    Some(self.owner_of_type(k + 2))
                } else if closure || bound {
                    Some(None)
                } else {
                    None
                };
                match says {
                    Some(None) => return None,
                    Some(Some(owner)) => {
                        if agreed.is_some_and(|a| a != owner) {
                            return None;
                        }
                        agreed = Some(owner);
                    }
                    None => {}
                }
            }
            k += 1;
        }
        agreed
    }

    fn owner_of_type(&self, mut k: usize) -> Option<Owner> {
        // Bounded: each turn moves `k` past one `&`, lifetime or `mut`.
        while matches!(self.tok(k), Some(t) if t.k == K::P(b'&') || t.k == K::Life) || self.is_word(self.tok(k), "mut") {
            k += 1;
        }
        self.owner_of_path(k).map(|(owner, _)| owner)
    }

    fn owner_of_init(&self, k: usize) -> Option<Owner> {
        if self.is_word(self.tok(k), "vec_in") && matches!(self.tok(k + 1), Some(t) if t.k == K::P(b'!')) {
            return Some(Owner::Vec);
        }
        if self.is_word(self.tok(k), "blaze_own")
            && self.colons_at(k + 1)
            && self.is_word(self.tok(k + 3), "vec_in")
            && matches!(self.tok(k + 4), Some(t) if t.k == K::P(b'!'))
        {
            return Some(Owner::Vec);
        }
        let (owner, last) = self.owner_of_path(k)?;
        self.colons_at(last + 1).then_some(owner)
    }

    /// A blaze-own collection type named at `k` (`AVec`, `ADeque`, `ASoa`, `FixedVec`, or
    /// `blaze_own::Vec` and the like), and its last token.
    fn owner_of_path(&self, k: usize) -> Option<(Owner, usize)> {
        let alias = |w: &str| match w {
            "AVec" => Some(Owner::Vec),
            "ADeque" => Some(Owner::Deque),
            "ASoa" => Some(Owner::Soa),
            "FixedVec" => Some(Owner::Fixed),
            _ => None,
        };
        let word = self.word(self.tok(k))?;
        if word == "blaze_own" && self.colons_at(k + 1) {
            let named = self.word(self.tok(k + 3))?;
            let owner = match named {
                "Vec" => Some(Owner::Vec),
                "VecDeque" => Some(Owner::Deque),
                "Soa" => Some(Owner::Soa),
                _ => alias(named),
            }?;
            return Some((owner, k + 3));
        }
        alias(word).map(|owner| (owner, k))
    }

    /// The owner of the path that ends at token `k` (before `::name(`).
    fn owner_of_path_end(&self, k: usize) -> Option<Owner> {
        if self.colons_before(k) {
            let (owner, last) = self.owner_of_path(k - 3)?;
            return (last == k).then_some(owner);
        }
        let (owner, last) = self.owner_of_path(k)?;
        (last == k).then_some(owner)
    }

    // ---- mut-ref (`&mut` / `ref mut` -> `&uniq` / `ref uniq`) ---------------------------

    /// Flags `&mut` and `&'a mut` unique references (types, receivers, expressions, patterns)
    /// and attaches a mechanical fix replacing `mut` with `uniq`. Ignores `&raw mut`, `*mut`,
    /// `let mut`, and raw identifiers (`r#mut`).
    fn mut_ref_amp(&mut self, i: usize, amp: Tok) {
        let Some(next) = self.tok(i + 1) else { return };
        let mut_tok = if next.k == K::Ident && next.word == W_MUT {
            next
        } else if next.k == K::Life
            && let Some(after_life) = self.tok(i + 2)
            && after_life.k == K::Ident
            && after_life.word == W_MUT
        {
            after_life
        } else {
            return;
        };
        self.cands.push(Cand {
            template: Template::MutRef,
            range: Span::new(amp.start, mut_tok.end),
            arg: None,
            plan: Plan::Replace { range: Span::new(mut_tok.start, mut_tok.end), text: "uniq" },
        });
    }

    /// Flags `ref mut` bindings and attaches a mechanical fix replacing `mut` with `uniq`.
    fn ref_mut_word(&mut self, i: usize, ref_tok: Tok) {
        let Some(next) = self.tok(i + 1) else { return };
        if next.k == K::Ident && next.word == W_MUT {
            self.cands.push(Cand {
                template: Template::RefMut,
                range: Span::new(ref_tok.start, next.end),
                arg: None,
                plan: Plan::Replace { range: Span::new(next.start, next.end), text: "uniq" },
            });
        }
    }

    /// Flags `let mut x = ..;` when `x` is never rebound (`x = ..`, `x += ..`, ...) in its
    /// enclosing block and is only borrowed uniquely (`&uniq x` / `&mut x`), attaching a
    /// mechanical fix that removes the redundant `mut ` keyword.
    fn let_mut_word(&mut self, i: usize, let_tok: Tok) {
        let Some(mut_tok) = self.tok(i + 1) else { return };
        if mut_tok.k != K::Ident || mut_tok.word != W_MUT {
            return;
        }
        let Some(name_tok) = self.tok(i + 2) else { return };
        if name_tok.k != K::Ident {
            return;
        }
        // Only simple `let mut x = ...;` or `let mut x: T = ...;` bindings (not destructuring).
        let Some(after_name) = self.tok(i + 3) else { return };
        if !matches!(after_name.k, K::P(b'=') | K::P(b':')) {
            return;
        }
        let name = self.str_of(name_tok);
        if name.is_empty() || name.starts_with('_') {
            return;
        }

        // Advance past the initializer statement's `;` at delimiter depth 0.
        let mut depth = 0i32;
        let mut j = i + 3;
        while let Some(tok) = self.tok(j) {
            match tok.k {
                K::P(b'{' | b'(' | b'[') => depth += 1,
                K::P(b'}' | b')' | b']') => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                }
                K::P(b';') if depth == 0 => {
                    j += 1;
                    break;
                }
                _ => {}
            }
            j += 1;
        }

        // Scan the rest of the enclosing block (`block_depth >= 0`) until it closes or `name` is shadowed.
        let mut uniq_borrowed = false;
        let mut rebound = false;
        let mut block_depth = 0i32;
        while let Some(tok) = self.tok(j) {
            match tok.k {
                K::P(b'{') => block_depth += 1,
                K::P(b'}') => {
                    if block_depth == 0 {
                        break;
                    }
                    block_depth -= 1;
                }
                K::Ident if tok.word == W_LET && block_depth == 0 => {
                    let next1 = self.tok(j + 1);
                    let shadow = if next1.is_some_and(|t| t.k == K::Ident && t.word == W_MUT) {
                        self.tok(j + 2)
                    } else {
                        next1
                    };
                    if shadow.is_some_and(|t| t.k == K::Ident && self.str_of(t) == name) {
                        break;
                    }
                }
                K::Ident if self.str_of(tok) == name => {
                    let prev = if j > 0 { self.tok(j - 1) } else { None };
                    let prev2 = if j > 1 { self.tok(j - 2) } else { None };
                    // Check `&uniq name` or `&mut name`.
                    let is_uniq_borrow = prev.is_some_and(|p| {
                        p.k == K::Ident && (p.word == W_UNIQ || p.word == W_MUT)
                    }) && prev2.is_some_and(|p2| p2.k == K::P(b'&'));
                    if is_uniq_borrow {
                        uniq_borrowed = true;
                    } else {
                        // Skip field/path access `.name` / `::name`.
                        let is_member = prev.is_some_and(|p| matches!(p.k, K::P(b'.' | b':')));
                        if !is_member {
                            if let Some(next) = self.tok(j + 1) {
                                let next2 = self.tok(j + 2);
                                // Bare rebinding: `name = expr` (not `==` / `=>`) or compound assign.
                                let is_eq = next.k == K::P(b'=')
                                    && !next2.is_some_and(|n2| matches!(n2.k, K::P(b'=' | b'>')));
                                let is_compound = matches!(
                                    next.k,
                                    K::P(b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^')
                                ) && next2.is_some_and(|n2| n2.k == K::P(b'='));
                                let is_shift_assign = matches!(next.k, K::P(b'<' | b'>'))
                                    && next2.is_some_and(|n2| n2.k == next.k)
                                    && self.tok(j + 3).is_some_and(|n3| n3.k == K::P(b'='));
                                if is_eq || is_compound || is_shift_assign {
                                    rebound = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            j += 1;
        }

        if uniq_borrowed && !rebound {
            self.cands.push(Cand {
                template: Template::MutForUniqBorrow,
                range: Span::new(let_tok.start, name_tok.end),
                arg: None,
                plan: Plan::Replace { range: Span::new(mut_tok.start, name_tok.start), text: "" },
            });
        }
    }

    // ---- magic-slice-len (`bytes.get(0..3) == Some(..)`, `.get(at..at + 2)`, etc.) ------

    /// Finds the matching closing delimiter (`open_b` / `close_b`) starting at `open_idx`.
    fn find_matching_delim(&self, open_idx: usize, open_b: u8, close_b: u8) -> Option<usize> {
        let mut depth = 0u32;
        let mut j = open_idx;
        while let Some(tok) = self.tok(j) {
            if tok.k == K::P(open_b) {
                depth += 1;
            } else if tok.k == K::P(close_b) {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(j);
                }
            }
            j += 1;
        }
        None
    }

    /// Parses a magic slice range inside tokens `[from..to)`:
    /// - `..N` / `0..N` (length `N`, `from_zero = true`)
    /// - `..=N` / `0..=N` (length `N + 1`, `from_zero = true`)
    /// - `pos..pos + N` (length `N`, `from_zero = false`)
    /// - `pos..=pos + N` (length `N + 1`, `from_zero = false`)
    /// Returns `Some((len, from_zero))` only when `N` is a bare integer literal with `len >= 1`.
    fn parse_magic_range(&self, from: usize, to: usize) -> Option<(usize, bool)> {
        if from >= to {
            return None;
        }
        // Locate `..` (two adjacent `.` tokens) inside `[from..to)`.
        let mut dd = None;
        let mut j = from;
        while j + 1 < to {
            if let (Some(a), Some(b)) = (self.tok(j), self.tok(j + 1))
                && a.k == K::P(b'.')
                && b.k == K::P(b'.')
                && a.end == b.start
            {
                dd = Some(j);
                break;
            }
            j += 1;
        }
        let dd = dd?;
        let mut rhs_start = dd + 2;
        let mut inclusive = false;
        if rhs_start < to
            && let (Some(dot2), Some(eq)) = (self.tok(dd + 1), self.tok(rhs_start))
            && eq.k == K::P(b'=')
            && dot2.end == eq.start
        {
            inclusive = true;
            rhs_start += 1;
        }
        let lhs_count = dd.saturating_sub(from);
        let rhs_count = to.saturating_sub(rhs_start);
        if rhs_count == 0 {
            return None;
        }
        // Case 1: `..N`, `0..N`, `..=N`, `0..=N`.
        let is_zero_start = lhs_count == 0
            || (lhs_count == 1
                && self.tok(from).is_some_and(|t| t.k == K::Int && parse_small_uint(self.str_of(t)) == Some(0)));
        if is_zero_start {
            if rhs_count == 1
                && let Some(end_tok) = self.tok(rhs_start)
                && end_tok.k == K::Int
                && let Some(n) = parse_small_uint(self.str_of(end_tok))
            {
                let len = if inclusive { n.checked_add(1)? } else { n };
                if len >= 1 {
                    return Some((len, true));
                }
            }
            return None;
        }
        // Case 2: `pos..pos + N` or `pos..=pos + N` (same identifier on both sides).
        if lhs_count == 1
            && rhs_count == 3
            && let (Some(l_id), Some(r_id), Some(plus), Some(n_tok)) =
                (self.tok(from), self.tok(rhs_start), self.tok(rhs_start + 1), self.tok(rhs_start + 2))
            && l_id.k == K::Ident
            && r_id.k == K::Ident
            && plus.k == K::P(b'+')
            && n_tok.k == K::Int
            && self.str_of(l_id) == self.str_of(r_id)
            && let Some(n) = parse_small_uint(self.str_of(n_tok))
        {
            let len = if inclusive { n.checked_add(1)? } else { n };
            if len >= 1 {
                return Some((len, false));
            }
        }
        None
    }

    /// Computes the static byte/element length of a literal or `&[e1, e2, ...]` slice expression
    /// over tokens `[from..to)`, when statically known.
    fn static_slice_rhs_len(&self, from: usize, to: usize) -> Option<usize> {
        if from >= to {
            return None;
        }
        // Unwrap a leading `&` or `*` (e.g. `&b"foo"[..]`, `*b"foo"`, `&[1, 2, 3]`).
        let mut s = from;
        while s < to && self.tok(s).is_some_and(|t| t.k == K::P(b'&') || t.k == K::P(b'*')) {
            s += 1;
        }
        // Strip a trailing `[..]` slice coercion if present (`&b"foo"[..]`).
        let mut e = to;
        if e >= s + 4
            && self.tok(e - 1).is_some_and(|t| t.k == K::P(b']'))
            && self.tok(e - 2).is_some_and(|t| t.k == K::P(b'.'))
            && self.tok(e - 3).is_some_and(|t| t.k == K::P(b'.'))
            && self.tok(e - 4).is_some_and(|t| t.k == K::P(b'['))
        {
            e -= 4;
        }
        if s >= e {
            return None;
        }
        if s + 1 == e
            && let Some(lit) = self.tok(s)
            && lit.k == K::Lit
        {
            return static_str_lit_byte_len(self.str_of(lit));
        }
        if self.tok(s).is_some_and(|t| t.k == K::P(b'['))
            && self.tok(e - 1).is_some_and(|t| t.k == K::P(b']'))
        {
            return self.count_array_elems(s + 1, e - 1);
        }
        None
    }

    /// Counts comma-separated top-level elements in an array literal `[a, b, c]`. Returns `None`
    /// for repeat syntax `[val; count]` or empty expressions that can't be counted simply.
    fn count_array_elems(&self, from: usize, to: usize) -> Option<usize> {
        if from >= to {
            return Some(0);
        }
        let mut count = 0usize;
        let mut saw_token_in_elem = false;
        let mut depth = 0u32;
        let mut j = from;
        while j < to {
            let Some(tok) = self.tok(j) else { break };
            match tok.k {
                K::P(b'(' | b'[' | b'{') => {
                    depth += 1;
                    saw_token_in_elem = true;
                }
                K::P(b')' | b']' | b'}') => {
                    depth = depth.saturating_sub(1);
                    saw_token_in_elem = true;
                }
                K::P(b';') if depth == 0 => return None,
                K::P(b',') if depth == 0 => {
                    if saw_token_in_elem {
                        count += 1;
                        saw_token_in_elem = false;
                    }
                }
                _ => {
                    saw_token_in_elem = true;
                }
            }
            j += 1;
        }
        if saw_token_in_elem {
            count += 1;
        }
        Some(count)
    }

    /// Returns `(is_eq, op_end_idx)` if tokens starting at `idx` are `==` or `!=`.
    fn eq_op_at(&self, idx: usize) -> Option<(bool, usize)> {
        let (a, b) = (self.tok(idx)?, self.tok(idx + 1)?);
        if a.end != b.start || b.k != K::P(b'=') {
            return None;
        }
        match a.k {
            K::P(b'=') => Some((true, idx + 2)),
            K::P(b'!') => Some((false, idx + 2)),
            _ => None,
        }
    }

    /// Walks back one token to include the simple receiver identifier before `.get(...)` when
    /// available, solely for a clearer diagnostic span.
    fn receiver_start_before_dot(&self, dot_idx: usize, fallback: u32) -> u32 {
        if dot_idx > 0
            && let Some(prev) = self.tok(dot_idx - 1)
            && prev.k == K::Ident
        {
            prev.start
        } else {
            fallback
        }
    }

    /// Checks `.get(0..N)`, `.get(..N)`, `.get(pos..pos + N)`, `.get(pos..=pos + N)`:
    /// - Followed by `== Some(RHS)` / `!= Some(RHS)` or `.is_some_and(|x| x == RHS)` ->
    ///   `MagicSlicePrefix` (or `MagicSlicePrefixMismatch` if length mismatches literal), with a
    ///   mechanical `starts_with(` auto-fix for `0..N`/`..N` + `== Some(RHS)`.
    /// - Followed by `.try_into()` or a fixed byte-width (`2`, `4`, `8`, `16`) -> `MagicSliceBytes`.
    /// - Otherwise -> `MagicSliceRange`.
    fn magic_slice_get(&mut self, i: usize, get_tok: Tok) {
        if i == 0 || !self.tok(i - 1).is_some_and(|t| t.k == K::P(b'.')) {
            return;
        }
        // Ignore `..get(` (impossible, but guard against range dot).
        if i >= 2
            && let (Some(a), Some(b)) = (self.tok(i - 2), self.tok(i - 1))
            && a.k == K::P(b'.')
            && a.end == b.start
        {
            return;
        }
        let open_idx = i + 1;
        if !self.tok(open_idx).is_some_and(|t| t.k == K::P(b'(')) {
            return;
        }
        let Some(close_idx) = self.find_matching_delim(open_idx, b'(', b')') else {
            return;
        };
        let Some((range_len, from_zero)) = self.parse_magic_range(open_idx + 1, close_idx) else {
            return;
        };
        let Some(close_tok) = self.tok(close_idx) else { return };
        let start = self.receiver_start_before_dot(i - 1, get_tok.start);

        // Form A1: `.get(..) == Some(RHS)` or `!= Some(RHS)`.
        if let Some((is_eq, after_op)) = self.eq_op_at(close_idx + 1)
            && let (Some(some_tok), Some(some_open)) = (self.tok(after_op), self.tok(after_op + 1))
            && some_tok.k == K::Ident
            && some_tok.word == W_SOME
            && some_open.k == K::P(b'(')
            && let Some(some_close) = self.find_matching_delim(after_op + 1, b'(', b')')
            && some_close > after_op + 2
            && let (Some(rhs_first), Some(rhs_last), Some(some_close_tok)) =
                (self.tok(after_op + 2), self.tok(some_close - 1), self.tok(some_close))
        {
            let rhs_span = Span::new(rhs_first.start, rhs_last.end);
            let full_span = Span::new(start, some_close_tok.end);
            let lit_len = self.static_slice_rhs_len(after_op + 2, some_close);
            if let Some(expected) = lit_len
                && expected != range_len
            {
                self.cands.push(Cand {
                    template: Template::MagicSlicePrefixMismatch,
                    range: full_span,
                    arg: Some(Arg::Span(rhs_span)),
                    plan: Plan::None,
                });
                return;
            }
            let plan = if from_zero && is_eq {
                Plan::Replace { range: Span::new(get_tok.start, some_open.end), text: "starts_with(" }
            } else {
                Plan::None
            };
            self.cands.push(Cand {
                template: Template::MagicSlicePrefix,
                range: full_span,
                arg: Some(Arg::Span(rhs_span)),
                plan,
            });
            return;
        }

        // Form A2: `.get(..).is_some_and(|x| x == RHS)`.
        if let (Some(dot), Some(isa), Some(isa_open)) =
            (self.tok(close_idx + 1), self.tok(close_idx + 2), self.tok(close_idx + 3))
            && dot.k == K::P(b'.')
            && isa.k == K::Ident
            && isa.word == W_IS_SOME_AND
            && isa_open.k == K::P(b'(')
            && let Some(isa_close) = self.find_matching_delim(close_idx + 3, b'(', b')')
            && let Some(isa_close_tok) = self.tok(isa_close)
        {
            // Look for `|id| id == RHS` inside `is_some_and(...)`.
            let b0 = close_idx + 4;
            if let (Some(bar1), Some(param), Some(bar2), Some(lhs_id)) =
                (self.tok(b0), self.tok(b0 + 1), self.tok(b0 + 2), self.tok(b0 + 3))
                && bar1.k == K::P(b'|')
                && param.k == K::Ident
                && bar2.k == K::P(b'|')
                && lhs_id.k == K::Ident
                && self.str_of(param) == self.str_of(lhs_id)
                && let Some((_, rhs_start)) = self.eq_op_at(b0 + 4)
                && rhs_start < isa_close
                && let (Some(rhs_first), Some(rhs_last)) = (self.tok(rhs_start), self.tok(isa_close - 1))
            {
                let rhs_span = Span::new(rhs_first.start, rhs_last.end);
                let full_span = Span::new(start, isa_close_tok.end);
                let template = match self.static_slice_rhs_len(rhs_start, isa_close) {
                    Some(expected) if expected != range_len => Template::MagicSlicePrefixMismatch,
                    _ => Template::MagicSlicePrefix,
                };
                let is_eq = self.eq_op_at(b0 + 4).is_some_and(|(eq, _)| eq);
                let rhs_is_ref_or_lit = rhs_first.k == K::Lit || rhs_first.k == K::P(b'&');
                let plan = if template == Template::MagicSlicePrefix && from_zero && is_eq && rhs_is_ref_or_lit {
                    Plan::Replace { range: Span::new(get_tok.start, rhs_first.start), text: "starts_with(" }
                } else {
                    Plan::None
                };
                self.cands.push(Cand { template, range: full_span, arg: Some(Arg::Span(rhs_span)), plan });
                return;
            }
        }

        // Form C: standalone `.get(0..N)` / `.get(..N)` / `.get(pos..pos + N)`.
        let mut after = close_idx + 1;
        if self.tok(after).is_some_and(|t| t.k == K::P(b'?')) {
            after += 1;
        }
        let feeds_try_into = self.tok(after).is_some_and(|t| t.k == K::P(b'.'))
            && self.tok(after + 1).is_some_and(|t| t.k == K::Ident && t.word == W_TRY_INTO);
        let template = if feeds_try_into || matches!(range_len, 2 | 4 | 8 | 16) {
            Template::MagicSliceBytes
        } else {
            Template::MagicSliceRange
        };
        self.cands.push(Cand { template, range: Span::new(start, close_tok.end), arg: None, plan: Plan::None });
    }

    /// Checks `&x[0..N] == RHS`, `&x[..N] == RHS`, `&x[pos..pos + N] == RHS` (and `!=`).
    fn magic_slice_amp_index(&mut self, i: usize, amp: Tok) {
        let Some(recv) = self.tok(i + 1) else { return };
        if recv.k != K::Ident {
            return;
        }
        let open_idx = i + 2;
        if !self.tok(open_idx).is_some_and(|t| t.k == K::P(b'[')) {
            return;
        }
        let Some(close_idx) = self.find_matching_delim(open_idx, b'[', b']') else {
            return;
        };
        let Some((range_len, from_zero)) = self.parse_magic_range(open_idx + 1, close_idx) else {
            return;
        };
        let Some((is_eq, rhs_start)) = self.eq_op_at(close_idx + 1) else {
            return;
        };
        // Find end of a simple RHS expression: literal `b"..."` / `"..."`, `&[...]`, or ident.
        let (rhs_end, rhs_span, rhs_is_ref_or_lit) = if let Some(first) = self.tok(rhs_start) {
            if first.k == K::Lit {
                (rhs_start + 1, Span::new(first.start, first.end), true)
            } else if first.k == K::P(b'&')
                && self.tok(rhs_start + 1).is_some_and(|t| t.k == K::P(b'['))
                && let Some(arr_close) = self.find_matching_delim(rhs_start + 1, b'[', b']')
                && let Some(end_tok) = self.tok(arr_close)
            {
                (arr_close + 1, Span::new(first.start, end_tok.end), true)
            } else if first.k == K::Ident {
                (rhs_start + 1, Span::new(first.start, first.end), false)
            } else {
                return;
            }
        } else {
            return;
        };
        let template = match self.static_slice_rhs_len(rhs_start, rhs_end) {
            Some(expected) if expected != range_len => Template::MagicSlicePrefixMismatch,
            _ => Template::MagicSlicePrefix,
        };
        let plan = if template == Template::MagicSlicePrefix
            && from_zero
            && is_eq
            && rhs_is_ref_or_lit
            && let Some(open_tok) = self.tok(open_idx)
        {
            Plan::WrapStartsWith {
                amp: Span::new(amp.start, amp.end),
                middle: Span::new(open_tok.start, rhs_span.start),
                after_rhs: rhs_span.end,
            }
        } else {
            Plan::None
        };
        self.cands.push(Cand {
            template,
            range: Span::new(amp.start, rhs_span.end),
            arg: Some(Arg::Span(rhs_span)),
            plan,
        });
    }

    /// Checks `x.starts_with(LIT)` followed immediately in its `{ ... }` body by a hardcoded
    /// advance (`+= N`, `= N`, or `[N..]`) where `N` equals `LIT`'s static byte length.
    fn magic_slice_starts_with(&mut self, i: usize, _sw: Tok) {
        if i == 0 || !self.tok(i - 1).is_some_and(|t| t.k == K::P(b'.')) {
            return;
        }
        let open_idx = i + 1;
        if !self.tok(open_idx).is_some_and(|t| t.k == K::P(b'(')) {
            return;
        }
        let Some(close_idx) = self.find_matching_delim(open_idx, b'(', b')') else {
            return;
        };
        if close_idx <= open_idx + 1 {
            return;
        }
        let Some(lit_len) = self.static_slice_rhs_len(open_idx + 1, close_idx) else {
            return;
        };
        if lit_len == 0 {
            return;
        }
        let (Some(arg_first), Some(arg_last)) = (self.tok(open_idx + 1), self.tok(close_idx - 1)) else {
            return;
        };
        let prefix_span = Span::new(arg_first.start, arg_last.end);
        // Only look into an immediate `{ ... }` block right after `starts_with(LIT)`.
        let brace_idx = close_idx + 1;
        if !self.tok(brace_idx).is_some_and(|t| t.k == K::P(b'{')) {
            return;
        }
        let Some(brace_close) = self.find_matching_delim(brace_idx, b'{', b'}') else {
            return;
        };
        // Scan up to the first 16 tokens of the block at depth 0 for `[N..]` or `+= N` / `= N`.
        let limit = brace_close.min(brace_idx + 20);
        let mut j = brace_idx + 1;
        while j < limit {
            let Some(tok) = self.tok(j) else { break };
            // Pattern 1: `[N..]` where `N == lit_len`.
            if tok.k == K::P(b'[')
                && let (Some(n_tok), Some(d1), Some(d2), Some(rb)) =
                    (self.tok(j + 1), self.tok(j + 2), self.tok(j + 3), self.tok(j + 4))
                && n_tok.k == K::Int
                && parse_small_uint(self.str_of(n_tok)) == Some(lit_len)
                && d1.k == K::P(b'.')
                && d2.k == K::P(b'.')
                && d1.end == d2.start
                && rb.k == K::P(b']')
            {
                self.cands.push(Cand {
                    template: Template::MagicSliceAdvance,
                    range: Span::new(tok.start, rb.end),
                    arg: Some(Arg::Span(prefix_span)),
                    plan: Plan::None,
                });
                return;
            }
            // Pattern 2: `at += N;` or `at = N;` where `N == lit_len`.
            if tok.k == K::Ident
                && let (Some(op1), Some(op2), Some(n_tok)) = (self.tok(j + 1), self.tok(j + 2), self.tok(j + 3))
                && op1.k == K::P(b'+')
                && op2.k == K::P(b'=')
                && op1.end == op2.start
                && n_tok.k == K::Int
                && parse_small_uint(self.str_of(n_tok)) == Some(lit_len)
            {
                self.cands.push(Cand {
                    template: Template::MagicSliceAdvance,
                    range: Span::new(tok.start, n_tok.end),
                    arg: Some(Arg::Span(prefix_span)),
                    plan: Plan::None,
                });
                return;
            }
            j += 1;
        }
    }

    // ---- bool-as-int (`(a == b) as u32`, `x.is_some() as usize` -> `T::from(..)`) -------

    /// Finds the matching opening delimiter walking backwards from `close_idx`.
    fn find_matching_open(&self, close_idx: usize, open_b: u8, close_b: u8) -> Option<usize> {
        let mut depth = 0u32;
        let mut j = close_idx;
        loop {
            let tok = self.tok(j)?;
            if tok.k == K::P(close_b) {
                depth += 1;
            } else if tok.k == K::P(open_b) {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(j);
                }
            }
            if j == 0 {
                return None;
            }
            j -= 1;
        }
    }

    /// True if tokens inside `[from..to)` contain a top-level boolean comparison/logical operator
    /// (`==`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||`). Note: bare `!x` is intentionally NOT
    /// matched here because `!` is bitwise NOT on integer types.
    fn is_bool_expr_tokens(&self, from: usize, to: usize) -> bool {
        if from >= to {
            return false;
        }
        let mut depth = 0u32;
        let mut turbo_depth = 0u32;
        let mut j = from;
        while j < to {
            let Some(tok) = self.tok(j) else { break };
            match tok.k {
                K::P(b'(' | b'[' | b'{') => depth += 1,
                K::P(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                K::P(b'<') if depth == 0 => {
                    // Distinguish comparison `<` from turbofish `::<` (preceded by `::`) and `<<`.
                    let is_turbo = j >= 2
                        && self.tok(j - 1).is_some_and(|t| t.k == K::P(b':'))
                        && self.tok(j - 2).is_some_and(|t| t.k == K::P(b':'));
                    let is_shl = self.tok(j + 1).is_some_and(|t| t.k == K::P(b'<') && tok.end == t.start);
                    if is_turbo || turbo_depth > 0 {
                        turbo_depth += 1;
                    } else if is_shl {
                        j += 1;
                    } else {
                        return true;
                    }
                }
                K::P(b'>') if depth == 0 => {
                    if turbo_depth > 0 {
                        turbo_depth -= 1;
                    } else {
                        let is_shr = self.tok(j + 1).is_some_and(|t| t.k == K::P(b'>') && tok.end == t.start);
                        let is_arrow = j > from && self.tok(j - 1).is_some_and(|t| (t.k == K::P(b'-') || t.k == K::P(b'=')) && t.end == tok.start);
                        if !is_shr && !is_arrow {
                            return true;
                        }
                        if is_shr {
                            j += 1;
                        }
                    }
                }
                K::P(b'=' | b'!') if depth == 0 => {
                    if self.tok(j + 1).is_some_and(|t| t.k == K::P(b'=') && tok.end == t.start) {
                        return true;
                    }
                }
                K::P(b'&') if depth == 0 => {
                    if self.tok(j + 1).is_some_and(|t| t.k == K::P(b'&') && tok.end == t.start) {
                        return true;
                    }
                }
                K::P(b'|') if depth == 0 => {
                    if self.tok(j + 1).is_some_and(|t| t.k == K::P(b'|') && tok.end == t.start) {
                        return true;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        false
    }

    /// True if `at` sits inside a `const fn` or `const NAME: ... =` item (where non-const
    /// `T::from(..)` cannot be called).
    fn inside_const_context(&self, at: usize) -> bool {
        let mut depth = 0u32;
        let mut j = at;
        while j > 0 {
            j -= 1;
            let Some(tok) = self.tok(j) else { break };
            match tok.k {
                K::P(b'}') => depth += 1,
                K::P(b'{') => {
                    if depth > 0 {
                        depth -= 1;
                    } else {
                        // Look back up to 16 tokens before `{` for `const fn`.
                        let lo = j.saturating_sub(24);
                        let mut k = j;
                        while k > lo {
                            k -= 1;
                            if let Some(t) = self.tok(k)
                                && t.k == K::Ident
                                && t.word == W_FN
                            {
                                return k > 0
                                    && self.tok(k - 1).is_some_and(|p| p.k == K::Ident && self.str_of(p) == "const");
                            }
                        }
                        return false;
                    }
                }
                K::P(b';') if depth == 0 => return false,
                K::Ident if depth == 0 && self.str_of(tok) == "const" => return true,
                _ => {}
            }
        }
        false
    }

    /// True if `at` sits directly inside a function named `target_name` (e.g. `fn is_empty`).
    fn inside_fn_named(&self, at: usize, target_name: &str) -> bool {
        let mut depth = 0u32;
        let mut j = at;
        while j > 0 {
            j -= 1;
            let Some(tok) = self.tok(j) else { break };
            match tok.k {
                K::P(b'}') => depth += 1,
                K::P(b'{') => {
                    if depth > 0 {
                        depth -= 1;
                    } else {
                        let lo = j.saturating_sub(24);
                        let mut k = j;
                        while k > lo {
                            k -= 1;
                            if let Some(t) = self.tok(k)
                                && t.k == K::Ident
                                && t.word == W_FN
                            {
                                return self
                                    .tok(k + 1)
                                    .is_some_and(|n| n.k == K::Ident && self.str_of(n) == target_name);
                            }
                        }
                        return false;
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Flags provably boolean expressions cast via `as <int>` (`(a == b) as u32`, `(!flag) as u8`,
    /// `x.is_some() as usize`, `true as u64`) and attaches a mechanical `T::from(..)` auto-fix.
    fn bool_as_int(&mut self, i: usize, _as_tok: Tok) {
        if i == 0 || self.in_use || self.inside_const_context(i) {
            return;
        }
        let Some(ty_tok) = self.tok(i + 1) else { return };
        if ty_tok.k != K::Ident {
            return;
        }
        let ty_name = self.str_of(ty_tok);
        let Some((prefix_paren, prefix_bare)) = int_from_prefixes(ty_name) else {
            return;
        };
        let Some(prev) = self.tok(i - 1) else { return };
        // Case 1: `(bool_expr) as T` -> `T::from(bool_expr)`.
        if prev.k == K::P(b')')
            && let Some(open_idx) = self.find_matching_open(i - 1, b'(', b')')
            && let Some(open_tok) = self.tok(open_idx)
        {
            // Subcase 1a: `recv.is_*() as T` -> `T::from(recv.is_*())`.
            if open_idx >= 2
                && let (Some(method), Some(dot)) = (self.tok(open_idx - 1), self.tok(open_idx - 2))
                && method.k == K::Ident
                && dot.k == K::P(b'.')
                && is_bool_method(self.str_of(method))
            {
                let expr_start = self.simple_chain_start(open_idx - 2);
                self.cands.push(Cand {
                    template: Template::BoolAsInt,
                    range: Span::new(expr_start, ty_tok.end),
                    arg: Some(Arg::Span(Span::new(ty_tok.start, ty_tok.end))),
                    plan: Plan::WrapFrom {
                        before_expr: expr_start,
                        after_expr: prev.end,
                        as_end: ty_tok.end,
                        prefix: prefix_bare,
                        add_close: true,
                    },
                });
                return;
            }
            // Subcase 1b: parenthesized bool expression `(a == b) as T`, `(!x) as T`.
            let preceded_by_ident = open_idx > 0
                && self.tok(open_idx - 1).is_some_and(|t| t.k == K::Ident);
            if !preceded_by_ident && self.is_bool_expr_tokens(open_idx + 1, i - 1) {
                self.cands.push(Cand {
                    template: Template::BoolAsInt,
                    range: Span::new(open_tok.start, ty_tok.end),
                    arg: Some(Arg::Span(Span::new(ty_tok.start, ty_tok.end))),
                    plan: Plan::WrapFrom {
                        before_expr: open_tok.start,
                        after_expr: prev.end,
                        as_end: ty_tok.end,
                        prefix: prefix_paren,
                        add_close: false,
                    },
                });
                return;
            }
        }
        // Case 2: `true as T` / `false as T`.
        if prev.k == K::Ident && (prev.word == W_TRUE || prev.word == W_FALSE) {
            self.cands.push(Cand {
                template: Template::BoolAsInt,
                range: Span::new(prev.start, ty_tok.end),
                arg: Some(Arg::Span(Span::new(ty_tok.start, ty_tok.end))),
                plan: Plan::WrapFrom {
                    before_expr: prev.start,
                    after_expr: prev.end,
                    as_end: ty_tok.end,
                    prefix: prefix_bare,
                    add_close: true,
                },
            });
        }
    }

    /// Walks back across simple `a.b.c` identifier/field chains before `dot_idx`.
    fn simple_chain_start(&self, dot_idx: usize) -> u32 {
        let mut cur = dot_idx;
        let mut best = self.tok(dot_idx).map_or(0, |t| t.start);
        while cur > 0 {
            let Some(prev) = self.tok(cur - 1) else { break };
            if prev.k == K::Ident {
                best = prev.start;
                if cur >= 2 && self.tok(cur - 2).is_some_and(|t| t.k == K::P(b'.')) {
                    cur -= 2;
                    continue;
                }
            }
            break;
        }
        best
    }

    // ---- manual-slice-idioms (`.len() == 0`, `for i in 0..xs.len()`, `.find` + split) ---

    /// Flags `x.len() == 0`, `x.len() > 0`, `x.len() != 0`, `x.len() >= 1` and attaches a
    /// mechanical `is_empty()` / `!is_empty()` auto-fix.
    fn manual_len_zero(&mut self, i: usize, len_tok: Tok) {
        if i == 0 || !self.tok(i - 1).is_some_and(|t| t.k == K::P(b'.')) {
            return;
        }
        // Never rewrite `self.len() == 0` inside `fn is_empty(&self)` (would cause infinite recursion!).
        if i >= 2
            && self.tok(i - 2).is_some_and(|t| t.k == K::Ident && t.word == W_SELF)
            && self.inside_fn_named(i, "is_empty")
        {
            return;
        }
        if !self.tok(i + 1).is_some_and(|t| t.k == K::P(b'('))
            || !self.tok(i + 2).is_some_and(|t| t.k == K::P(b')'))
        {
            return;
        }
        let op_idx = i + 3;
        let (is_empty, rhs_tok, after_rhs_idx) = if let Some((is_eq, rhs_i)) = self.eq_op_at(op_idx)
            && let Some(rhs) = self.tok(rhs_i)
            && rhs.k == K::Int
            && parse_small_uint(self.str_of(rhs)) == Some(0)
        {
            (is_eq, rhs, rhs_i + 1)
        } else if let (Some(gt), Some(next)) = (self.tok(op_idx), self.tok(op_idx + 1))
            && gt.k == K::P(b'>')
            && next.k == K::Int
            && parse_small_uint(self.str_of(next)) == Some(0)
        {
            (false, next, op_idx + 2)
        } else if let (Some(gt), Some(eq), Some(rhs)) = (self.tok(op_idx), self.tok(op_idx + 1), self.tok(op_idx + 2))
            && gt.k == K::P(b'>')
            && eq.k == K::P(b'=')
            && gt.end == eq.start
            && rhs.k == K::Int
            && parse_small_uint(self.str_of(rhs)) == Some(1)
        {
            (false, rhs, op_idx + 3)
        } else {
            return;
        };
        // Guard against compound RHS expressions like `bytes.len() >= 1 << 20` or `x.len() > 0 + n`.
        if self.tok(after_rhs_idx).is_some_and(|t| {
            matches!(t.k, K::P(b'+' | b'-' | b'*' | b'/' | b'%' | b'<' | b'>' | b'.' | b'^'))
                || (t.k == K::P(b'&') && !self.tok(after_rhs_idx + 1).is_some_and(|n| n.k == K::P(b'&') && t.end == n.start))
                || (t.k == K::P(b'|') && !self.tok(after_rhs_idx + 1).is_some_and(|n| n.k == K::P(b'|') && t.end == n.start))
        }) {
            return;
        }
        let recv_start = self.simple_chain_start(i - 1);
        // Guard against compound LHS expressions like `a.len() + b.len() >= 1`.
        let mut recv_tok_idx = i - 1;
        while recv_tok_idx > 0
            && self.tok(recv_tok_idx - 1).is_some_and(|t| t.start >= recv_start)
        {
            recv_tok_idx -= 1;
        }
        if recv_tok_idx > 0
            && self.tok(recv_tok_idx - 1).is_some_and(|t| {
                matches!(t.k, K::P(b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'<' | b'>'))
            })
        {
            return;
        }
        let can_negate = i >= 2
            && self.tok(i - 2).is_some_and(|t| t.k == K::Ident)
            && !self.tok(i - 1).is_some_and(|t| t.start == recv_start);
        let replace_span = Span::new(len_tok.start, rhs_tok.end);
        let plan = if is_empty {
            Plan::Replace { range: replace_span, text: "is_empty()" }
        } else if can_negate {
            Plan::NegateReplace { bang_at: recv_start, range: replace_span, text: "is_empty()" }
        } else {
            Plan::None
        };
        self.cands.push(Cand {
            template: Template::LenZeroCheck,
            range: Span::new(recv_start, rhs_tok.end),
            arg: None,
            plan,
        });
    }

    /// Flags `for idx in 0..xs.len() { ... xs[idx] ... }` index loops that cause unchecked index
    /// stops and redundant bounds checks.
    fn manual_index_loop(&mut self, i: usize, for_tok: Tok) {
        let (Some(idx_tok), Some(in_tok), Some(zero_tok), Some(d1), Some(d2)) =
            (self.tok(i + 1), self.tok(i + 2), self.tok(i + 3), self.tok(i + 4), self.tok(i + 5))
        else {
            return;
        };
        if idx_tok.k != K::Ident
            || in_tok.k != K::Ident
            || in_tok.word != W_IN
            || zero_tok.k != K::Int
            || parse_small_uint(self.str_of(zero_tok)) != Some(0)
            || d1.k != K::P(b'.')
            || d2.k != K::P(b'.')
            || d1.end != d2.start
        {
            return;
        }
        // Match `xs.len() {` after `0..`.
        let (Some(xs_tok), Some(dot), Some(len_tok), Some(lp), Some(rp), Some(lb)) = (
            self.tok(i + 6),
            self.tok(i + 7),
            self.tok(i + 8),
            self.tok(i + 9),
            self.tok(i + 10),
            self.tok(i + 11),
        ) else {
            return;
        };
        if xs_tok.k != K::Ident
            || dot.k != K::P(b'.')
            || len_tok.k != K::Ident
            || len_tok.word != W_LEN
            || lp.k != K::P(b'(')
            || rp.k != K::P(b')')
            || lb.k != K::P(b'{')
        {
            return;
        }
        let Some(rb_idx) = self.find_matching_delim(i + 11, b'{', b'}') else {
            return;
        };
        let idx_name = self.str_of(idx_tok);
        let xs_name = self.str_of(xs_tok);
        // Check if the loop body actually indexes `xs[idx]` (skipping `xs[idx] = ...` mutations
        // only? No, `for x in &mut xs` / `xs.iter_mut()` is also cleaner than `xs[idx] = ...`!).
        let mut j = i + 12;
        while j + 3 < rb_idx {
            if let (Some(a), Some(lb2), Some(b), Some(rb2)) =
                (self.tok(j), self.tok(j + 1), self.tok(j + 2), self.tok(j + 3))
                && a.k == K::Ident
                && lb2.k == K::P(b'[')
                && b.k == K::Ident
                && rb2.k == K::P(b']')
                && self.str_of(a) == xs_name
                && self.str_of(b) == idx_name
            {
                self.cands.push(Cand {
                    template: Template::IndexLoop,
                    range: Span::new(for_tok.start, rp.end),
                    arg: Some(Arg::Span(Span::new(xs_tok.start, xs_tok.end))),
                    plan: Plan::None,
                });
                return;
            }
            j += 1;
        }
    }

    /// Flags `let Some(pos) = s.find(pat)` / `Some(pos) => ...` followed by manual `&s[..pos]` or
    /// `&s[pos + 1..]` splitting where `s.split_once(pat)` derives both halves safely.
    fn manual_find_split(&mut self, i: usize, find_tok: Tok) {
        if i == 0 || !self.tok(i - 1).is_some_and(|t| t.k == K::P(b'.')) {
            return;
        }
        let open_idx = i + 1;
        if !self.tok(open_idx).is_some_and(|t| t.k == K::P(b'(')) {
            return;
        }
        let Some(close_idx) = self.find_matching_delim(open_idx, b'(', b')') else {
            return;
        };
        if close_idx <= open_idx + 1 {
            return;
        }
        let (Some(pat_first), Some(pat_last), Some(close_tok)) =
            (self.tok(open_idx + 1), self.tok(close_idx - 1), self.tok(close_idx))
        else {
            return;
        };
        // Only flag when the pattern is a single char or string literal (not a closure predicate).
        if pat_first.k != K::Lit || pat_first.start != pat_last.start {
            return;
        }
        let pat_span = Span::new(pat_first.start, pat_last.end);
        // Find bound variable `pos` in `Some(pos) = s.find(...)` just before `s.find(...)`.
        let mut bound_pos = None;
        let back_limit = i.saturating_sub(10);
        let mut b = i;
        while b > back_limit {
            b -= 1;
            if let (Some(some_t), Some(lp), Some(id), Some(rp)) =
                (self.tok(b), self.tok(b + 1), self.tok(b + 2), self.tok(b + 3))
                && some_t.k == K::Ident
                && some_t.word == W_SOME
                && lp.k == K::P(b'(')
                && id.k == K::Ident
                && rp.k == K::P(b')')
            {
                bound_pos = Some(self.str_of(id));
                break;
            }
        }
        let Some(pos_name) = bound_pos else {
            return;
        };
        // Look ahead in the next 32 tokens for `[..pos]` or `[pos + 1..]`.
        let scan_end = close_idx + 32;
        let mut j = close_idx + 1;
        while j < scan_end {
            let Some(tok) = self.tok(j) else { break };
            if tok.k == K::P(b'[') {
                // `[..pos]`
                if let (Some(d1), Some(d2), Some(id), Some(rb)) =
                    (self.tok(j + 1), self.tok(j + 2), self.tok(j + 3), self.tok(j + 4))
                    && d1.k == K::P(b'.')
                    && d2.k == K::P(b'.')
                    && d1.end == d2.start
                    && id.k == K::Ident
                    && self.str_of(id) == pos_name
                    && rb.k == K::P(b']')
                {
                    let start = self.receiver_start_before_dot(i - 1, find_tok.start);
                    self.cands.push(Cand {
                        template: Template::ManualFindSplit,
                        range: Span::new(start, close_tok.end),
                        arg: Some(Arg::Span(pat_span)),
                        plan: Plan::None,
                    });
                    return;
                }
                // `[pos + 1..]`
                if let (Some(id), Some(plus), Some(one), Some(d1), Some(d2), Some(rb)) = (
                    self.tok(j + 1),
                    self.tok(j + 2),
                    self.tok(j + 3),
                    self.tok(j + 4),
                    self.tok(j + 5),
                    self.tok(j + 6),
                ) && id.k == K::Ident
                    && self.str_of(id) == pos_name
                    && plus.k == K::P(b'+')
                    && one.k == K::Int
                    && d1.k == K::P(b'.')
                    && d2.k == K::P(b'.')
                    && d1.end == d2.start
                    && rb.k == K::P(b']')
                {
                    let start = self.receiver_start_before_dot(i - 1, find_tok.start);
                    self.cands.push(Cand {
                        template: Template::ManualFindSplit,
                        range: Span::new(start, close_tok.end),
                        arg: Some(Arg::Span(pat_span)),
                        plan: Plan::None,
                    });
                    return;
                }
            }
            j += 1;
        }
    }

    // ---- test-ambient-leak (`Alloc::global()` in tests, `#[cfg(test)] extern crate std;`)

    /// Flags `Alloc::global()` inside test code (trips `alloc_global_tests` ratchet).
    fn test_global_alloc(&mut self, i: usize, alloc_tok: Tok) {
        if !self.test_at(alloc_tok.start).meets(TestBits::ANY) {
            return;
        }
        if let (Some(c1), Some(c2), Some(glob), Some(lp), Some(rp)) =
            (self.tok(i + 1), self.tok(i + 2), self.tok(i + 3), self.tok(i + 4), self.tok(i + 5))
            && c1.k == K::P(b':')
            && c2.k == K::P(b':')
            && c1.end == c2.start
            && glob.k == K::Ident
            && glob.word == W_GLOBAL_FN
            && lp.k == K::P(b'(')
            && rp.k == K::P(b')')
        {
            self.cands.push(Cand {
                template: Template::TestGlobalAlloc,
                range: Span::new(alloc_tok.start, rp.end),
                arg: None,
                plan: Plan::None,
            });
        }
    }

    /// Flags `extern crate std;` inside test-gated items (`#[cfg(test)] extern crate std;`).
    fn test_extern_std(&mut self, i: usize, extern_tok: Tok) {
        if !self.test_at(extern_tok.start).meets(TestBits::ANY) {
            return;
        }
        if let (Some(krate), Some(std_tok)) = (self.tok(i + 1), self.tok(i + 2))
            && krate.k == K::Ident
            && krate.word == W_CRATE
            && std_tok.k == K::Ident
            && std_tok.word == W_STD
        {
            self.cands.push(Cand {
                template: Template::TestExternStd,
                range: Span::new(extern_tok.start, std_tok.end),
                arg: None,
                plan: Plan::None,
            });
        }
    }

    // ---- allocs' hatch ------------------------------------------------------------------

    /// allocs.rs's `keeps`: a line comment with `// alloc: <reason>`, the reason not empty.
    fn alloc_comment(&mut self, start: u32, end: u32) {
        let comment = &self.text[start as usize..end as usize];
        let mut rest = comment;
        // Bounded: each turn moves past one `//`.
        while let Some(at) = rest.find("//") {
            rest = &rest[at + 2..];
            let reason = rest.trim_start().strip_prefix("alloc").and_then(|it| it.strip_prefix(':')).map(str::trim_start);
            if reason.is_some_and(|it| !it.is_empty()) {
                self.push(Template::AllocComment, start, end, None);
                return;
            }
        }
    }

    // ---- the end ------------------------------------------------------------------------

    /// The candidates outside the test code their rule exempts, written out.
    fn emit(&mut self, out: &mut Findings<'_>) -> Result<(), LintError> {
        if self.cands.is_empty() {
            return Ok(());
        }
        let renames = self.rules.renames();
        for c in self.cands.iter() {
            let rule = c.template.rule();
            let exempt = rule.exempt();
            if self.test_at(c.range.start).meets(exempt) {
                continue;
            }
            let args_from = count(out.args.len())?;
            if let Some(arg) = c.arg {
                out.args.push(arg);
            }
            let args = Run { from: args_from, to: count(out.args.len())? };
            let fixes_from = count(out.fixes.len())?;
            match c.plan {
                Plan::None => {}
                Plan::Rename { row, name, after } => {
                    let rename = renames[row as usize];
                    let edits_from = count(out.edits.len())?;
                    out.push_edit(name, rename.new)?;
                    if after != u32::MAX {
                        out.push_edit(Span::new(after, after), rename.suffix)?;
                    }
                    out.fixes.push(Fix { edits: Run { from: edits_from, to: count(out.edits.len())? } });
                }
                Plan::Strip { tokens } => {
                    let edits_from = count(out.edits.len())?;
                    for token in tokens {
                        out.push_edit(token, "")?;
                    }
                    out.fixes.push(Fix { edits: Run { from: edits_from, to: count(out.edits.len())? } });
                }
                Plan::Replace { range, text } => {
                    let edits_from = count(out.edits.len())?;
                    out.push_edit(range, text)?;
                    out.fixes.push(Fix { edits: Run { from: edits_from, to: count(out.edits.len())? } });
                }
                Plan::WrapStartsWith { amp, middle, after_rhs } => {
                    let edits_from = count(out.edits.len())?;
                    out.push_edit(amp, "")?;
                    out.push_edit(middle, ".starts_with(")?;
                    out.push_edit(Span::new(after_rhs, after_rhs), ")")?;
                    out.fixes.push(Fix { edits: Run { from: edits_from, to: count(out.edits.len())? } });
                }
                Plan::WrapFrom { before_expr, after_expr, as_end, prefix, add_close } => {
                    let edits_from = count(out.edits.len())?;
                    out.push_edit(Span::new(before_expr, before_expr), prefix)?;
                    out.push_edit(Span::new(after_expr, as_end), if add_close { ")" } else { "" })?;
                    out.fixes.push(Fix { edits: Run { from: edits_from, to: count(out.edits.len())? } });
                }
                Plan::NegateReplace { bang_at, range, text } => {
                    let edits_from = count(out.edits.len())?;
                    out.push_edit(Span::new(bang_at, bang_at), "!")?;
                    out.push_edit(range, text)?;
                    out.fixes.push(Fix { edits: Run { from: edits_from, to: count(out.edits.len())? } });
                }
                Plan::ReplaceBuf { range, text } => {
                    let edits_from = count(out.edits.len())?;
                    let slice = self
                        .fix_buf
                        .get(text.range())
                        .and_then(|b| core::str::from_utf8(b).ok())
                        .unwrap_or("");
                    out.push_edit(range, slice)?;
                    out.fixes.push(Fix { edits: Run { from: edits_from, to: count(out.edits.len())? } });
                }
            }
            let fixes = Run { from: fixes_from, to: count(out.fixes.len())? };
            out.list.push(Finding { template: c.template, range: c.range, args, fixes });
        }
        Ok(())
    }
}

enum ItemEnd {
    At(u32),
    Under(u32),
    Open,
}

#[derive(Clone, Copy)]
enum Called {
    Bare,
    SelfModule,
    SelfType,
    SelfMethod,
    Other,
}

/// Returns `(prefix_for_parenthesized, prefix_for_bare)` for integer types supporting `From<bool>`.
fn int_from_prefixes(ty: &str) -> Option<(&'static str, &'static str)> {
    match ty {
        "u8" => Some(("u8::from", "u8::from(")),
        "u16" => Some(("u16::from", "u16::from(")),
        "u32" => Some(("u32::from", "u32::from(")),
        "u64" => Some(("u64::from", "u64::from(")),
        "u128" => Some(("u128::from", "u128::from(")),
        "usize" => Some(("usize::from", "usize::from(")),
        "i8" => Some(("i8::from", "i8::from(")),
        "i16" => Some(("i16::from", "i16::from(")),
        "i32" => Some(("i32::from", "i32::from(")),
        "i64" => Some(("i64::from", "i64::from(")),
        "i128" => Some(("i128::from", "i128::from(")),
        "isize" => Some(("isize::from", "isize::from(")),
        _ => None,
    }
}

/// Methods whose return type is unambiguously `bool`.
fn is_bool_method(name: &str) -> bool {
    matches!(
        name,
        "is_some"
            | "is_none"
            | "is_ok"
            | "is_err"
            | "is_empty"
            | "is_ascii"
            | "is_ascii_digit"
            | "is_ascii_alphabetic"
            | "is_ascii_alphanumeric"
            | "is_ascii_whitespace"
            | "is_ascii_hexdigit"
            | "is_sign_positive"
            | "is_sign_negative"
            | "is_nan"
            | "is_infinite"
            | "is_finite"
            | "is_power_of_two"
            | "starts_with"
            | "ends_with"
            | "contains"
    )
}

/// Parses a plain unsigned decimal integer literal (allowing `_` separators and optional
/// `usize`/`u32`/`u64` suffix). Returns `None` for hex/binary/octal or non-decimal tokens.
fn parse_small_uint(s: &str) -> Option<usize> {
    if s.starts_with("0x") || s.starts_with("0X") || s.starts_with("0b") || s.starts_with("0B") || s.starts_with("0o") || s.starts_with("0O") {
        return None;
    }
    let digits = s
        .strip_suffix("usize")
        .or_else(|| s.strip_suffix("u32"))
        .or_else(|| s.strip_suffix("u64"))
        .or_else(|| s.strip_suffix("u16"))
        .or_else(|| s.strip_suffix("u8"))
        .unwrap_or(s);
    if digits.is_empty() {
        return None;
    }
    let mut val = 0usize;
    let mut saw = false;
    for b in digits.bytes() {
        if b == b'_' {
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        saw = true;
        val = val.checked_mul(10)?.checked_add(usize::from(b - b'0'))?;
    }
    saw.then_some(val)
}

/// Computes the exact byte length of a Rust string or byte-string literal (`"..."`, `b"..."`,
/// `r#"..."#`, `br#"..."#`) when statically decidable. Returns `None` for non-string literals or
/// unrecognized escape sequences.
fn static_str_lit_byte_len(lit: &str) -> Option<usize> {
    // Raw strings / raw byte strings: `r"..."`, `r#"..."#`, `br"..."`, `br#"..."#`.
    let rest = lit.strip_prefix('b').unwrap_or(lit);
    if let Some(after_r) = rest.strip_prefix('r') {
        let hashes = after_r.bytes().take_while(|&b| b == b'#').count();
        let inner = after_r.get(hashes..)?.strip_prefix('"')?;
        let end = inner.len().checked_sub(1 + hashes)?;
        let content = inner.get(..end)?;
        return Some(content.len());
    }
    let inner = rest.strip_prefix('"')?.strip_suffix('"')?;
    let bytes = inner.as_bytes();
    let mut i = 0usize;
    let mut len = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'\\' {
            let Some(&esc) = bytes.get(i + 1) else { return None };
            match esc {
                b'n' | b'r' | b't' | b'\\' | b'0' | b'\'' | b'"' => {
                    len += 1;
                    i += 2;
                }
                b'x' => {
                    // `\xHH` is always 1 byte in byte strings and ASCII in strings (if < 0x80).
                    const HEX_ESCAPE_LEN: usize = 4;
                    if bytes.len().saturating_sub(i) < HEX_ESCAPE_LEN {
                        return None;
                    }
                    len += 1;
                    i += HEX_ESCAPE_LEN;
                }
                _ => return None,
            }
        } else {
            len += 1;
            i += 1;
        }
    }
    Some(len)
}

/// A type name that holds interior state: a cell (`Cell`, `RefCell`, `UnsafeCell`, `OnceCell`,
/// `LazyCell`), a lock (`Mutex`, `RwLock`, `OnceLock`, `LazyLock`, any `..Lock`), an atomic,
/// a once or lazy value, a condition variable. scripts/src/bin/ambient/sansio.rs's `statics`
/// column reads the same names.
fn stateful(word: &str) -> bool {
    word.starts_with("Atomic")
        || word.ends_with("Cell")
        || word.ends_with("Lock")
        || matches!(word, "Mutex" | "Condvar" | "Lazy" | "Once" | "Barrier")
}

/// True when `bytes` (an inter-hole run) contains only ASCII whitespace — and, when
/// `skip_line_comments` is true (`!rules.runs(Rule::Allocs)`), `//` line comments — so
/// `blaze_lex::tokenize` would emit zero kept tokens and zero `AllocComment` callbacks.
#[inline]
fn is_ignorable_gap(bytes: &[u8], skip_line_comments: bool) -> bool {
    let mut i = 0;
    // Bounded: each turn advances `i` by at least one byte.
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\n' | b'\r' | b'\t' => i += 1,
            b'/' if skip_line_comments && bytes.get(i + 1) == Some(&b'/') => {
                match memchr(b'\n', &bytes[i + 2..]) {
                    Some(nl) => i += 2 + nl + 1,
                    None => return true,
                }
            }
            _ => return false,
        }
    }
    true
}

/// A string literal's text (`"x"`, `r"x"`, `r#"x"#`) has something in it but whitespace: a
/// `lock_exception`'s `why` or a `blaze::trust`'s `reason`. A byte or C string, or a char, is
/// not one.
pub(crate) fn says_something(literal: &str) -> bool {
    let body = match literal.strip_prefix('r') {
        Some(raw) => raw.trim_matches('#'),
        None => literal,
    };
    match body.strip_prefix('"').and_then(|it| it.strip_suffix('"')) {
        Some(inner) => !inner.trim().is_empty(),
        None => false,
    }
}

// ---- marks and attributes, on text ------------------------------------------------------

/// clones.rs's `// clone:` mark on `line`, and what its words say.
fn clone_mark(line: &str) -> Option<Mark> {
    let at = line.find(CLONE_MARK)?;
    let why = line[at + CLONE_MARK.len()..].trim();
    let says = |words: &[&str], phrases: &[&str]| {
        why.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .any(|w| words.iter().any(|it| it.eq_ignore_ascii_case(w)))
            || phrases.iter().any(|p| contains_ignore_case(why, p))
    };
    Some(if says(&HANDLE_WORDS, &HANDLE_PHRASES) {
        Mark::Handle
    } else if says(&ERROR_WORDS, &ERROR_PHRASES) {
        Mark::ErrorPath
    } else if says(&DEEP_WORDS, &[]) {
        Mark::Deep
    } else {
        Mark::Unsorted
    })
}

/// A `// SAFETY:` comment on `line` (any case, after `//` and spaces).
fn safety_mark(line: &str) -> Option<Mark> {
    let mut rest = line;
    // Bounded: each turn moves past one `//`.
    while let Some(at) = rest.find("//") {
        rest = &rest[at + 2..];
        let said = rest.trim_start_matches(['/', '!']).trim_start();
        if said.len() >= 6 && said.as_bytes()[..6].eq_ignore_ascii_case(b"safety") {
            return Some(Mark::Safety);
        }
    }
    None
}

/// A `// seqcst: <why>` comment on `line` (any case, non-empty reason after `:`).
fn seqcst_mark(line: &str) -> Option<Mark> {
    let mut rest = line;
    // Bounded: each turn moves past one `//`.
    while let Some(at) = rest.find("//") {
        rest = &rest[at + 2..];
        let said = rest.trim_start_matches(['/', '!']).trim_start();
        if said.len() >= 7
            && said.as_bytes()[..6].eq_ignore_ascii_case(b"seqcst")
            && let Some(why) = said[6..].strip_prefix(':').map(str::trim)
            && !why.is_empty()
        {
            return Some(Mark::SeqCst);
        }
    }
    None
}

fn contains_ignore_case(text: &str, needle: &str) -> bool {
    let (text, needle) = (text.as_bytes(), needle.as_bytes());
    needle.is_empty() || text.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The ratchet's `cfg_test` (`inner`: `inner_cfg_test`) at the start of `rest`: the attribute's
/// length.
fn cfg_test_end(rest: &str, inner: bool) -> Option<usize> {
    let open = if inner { "#![cfg(" } else { "#[cfg(" };
    let r = rest.strip_prefix(open)?;
    if r.starts_with("test)]") {
        return Some(open.len() + "test)]".len());
    }
    let inside = r.strip_prefix("all(test")?;
    if inside.bytes().next().is_some_and(is_word_byte) {
        return None;
    }
    let close = inside.find(']')?;
    (close > 0 && inside.as_bytes()[close - 1] == b')').then(|| open.len() + "all(test".len() + close + 1)
}

/// The ratchet's `test_fn` at the start of `rest`: `#[(\w+::)*test\b[^\]]*\]`.
fn test_attr_end(rest: &str) -> Option<usize> {
    let r = rest.strip_prefix("#[")?;
    let bytes = r.as_bytes();
    let mut from = 0;
    // Bounded: each turn reads one word of the path and moves past it.
    let last = loop {
        let len = bytes[from..].iter().take_while(|b| is_word_byte(**b)).count();
        if len == 0 {
            return None;
        }
        let word = &r[from..from + len];
        from += len;
        match r[from..].strip_prefix("::") {
            Some(_) => from += 2,
            None => break word,
        }
    };
    if last != "test" {
        return None;
    }
    let close = r[from..].find(']')?;
    Some(2 + from + close + 1)
}

/// allocs.rs's `allowed`: `#!?\[[^\]]*\b(?:allow|expect)\([^\]]*\bblaze::implicit_allocator\b`
/// at the start of `rest`; the attribute's length to its `]`.
fn expects_implicit_allocator(rest: &str) -> Option<usize> {
    let r = rest.strip_prefix('#')?;
    let skip = usize::from(r.starts_with('!'));
    let r = r[skip..].strip_prefix('[')?;
    let close = r.find(']').unwrap_or(r.len());
    let inside = &r[..close];
    let bytes = inside.as_bytes();
    let starts_word = |at: usize| at == 0 || !is_word_byte(bytes[at - 1]);
    let mut opened = None;
    for word in ["allow(", "expect("] {
        let mut from = 0;
        // Bounded: each turn moves past one match.
        while let Some(n) = inside[from..].find(word) {
            let at = from + n;
            if starts_word(at) {
                opened = Some(opened.map_or(at + word.len(), |o: usize| o.min(at + word.len())));
                break;
            }
            from = at + 1;
        }
    }
    let opened = opened?;
    const LINT: &str = "blaze::implicit_allocator";
    let mut from = opened;
    // Bounded: each turn moves past one match.
    while let Some(n) = inside[from..].find(LINT) {
        let at = from + n;
        let end = at + LINT.len();
        if starts_word(at) && bytes.get(end).is_none_or(|b| !is_word_byte(*b)) {
            return Some(1 + skip + 1 + close.min(r.len()) + usize::from(close < r.len()));
        }
        from = at + 1;
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrimKind {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    F32,
    F64,
    Str,
    String,
}

#[derive(Clone, Copy)]
struct Receiver<'u> {
    name: &'u str,
    is_field: bool,
}

#[derive(Clone, Copy)]
struct TypeArgs {
    count: usize,
    first: (usize, usize),
    last: (usize, usize),
}

#[derive(Clone, Copy)]
struct ParsedParam {
    name_span: Span,
    ty_span: Span,
    prim: Option<PrimKind>,
    is_numeric: bool,
}

#[derive(Clone, Copy)]
struct PrimClass {
    kind: Option<PrimKind>,
    is_numeric: bool,
}

impl PrimClass {
    const NONE: PrimClass = PrimClass { kind: None, is_numeric: false };
}

#[inline]
fn classify_primitive(name: &str) -> PrimClass {
    match name {
        "bool" => PrimClass { kind: Some(PrimKind::Bool), is_numeric: false },
        "u8" => PrimClass { kind: Some(PrimKind::U8), is_numeric: true },
        "u16" => PrimClass { kind: Some(PrimKind::U16), is_numeric: true },
        "u32" => PrimClass { kind: Some(PrimKind::U32), is_numeric: true },
        "u64" => PrimClass { kind: Some(PrimKind::U64), is_numeric: true },
        "u128" => PrimClass { kind: Some(PrimKind::U128), is_numeric: true },
        "usize" => PrimClass { kind: Some(PrimKind::Usize), is_numeric: true },
        "i8" => PrimClass { kind: Some(PrimKind::I8), is_numeric: true },
        "i16" => PrimClass { kind: Some(PrimKind::I16), is_numeric: true },
        "i32" => PrimClass { kind: Some(PrimKind::I32), is_numeric: true },
        "i64" => PrimClass { kind: Some(PrimKind::I64), is_numeric: true },
        "i128" => PrimClass { kind: Some(PrimKind::I128), is_numeric: true },
        "isize" => PrimClass { kind: Some(PrimKind::Isize), is_numeric: true },
        "f32" => PrimClass { kind: Some(PrimKind::F32), is_numeric: true },
        "f64" => PrimClass { kind: Some(PrimKind::F64), is_numeric: true },
        "String" => PrimClass { kind: Some(PrimKind::String), is_numeric: false },
        _ => PrimClass::NONE,
    }
}

/// Well-known symmetric/coordinate/range parameter pairs where adjacent identical primitives are
/// idiomatic rather than a swappable-parameter defect.
#[inline]
fn is_symmetric_param_pair(a: &str, b: &str) -> bool {
    matches!(
        (a, b),
        ("x", "y")
            | ("y", "z")
            | ("z", "w")
            | ("a", "b")
            | ("b", "c")
            | ("lhs", "rhs")
            | ("left", "right")
            | ("lo", "hi")
            | ("low", "high")
            | ("min", "max")
            | ("start", "end")
            | ("from", "to")
            | ("first", "last")
            | ("row", "col")
            | ("width", "height")
            | ("w", "h")
            | ("r", "g")
            | ("g", "b")
            | ("b", "a")
            | ("u", "v")
            | ("dx", "dy")
            | ("p", "q")
    )
}

/// Whether a numeric parameter's name encodes a physical unit, time unit, or entity identity that
/// should be a zero-cost newtype (`struct TimeoutNs(u64)`, `struct BlockId(u32)`, ...).
#[inline]
fn carries_primitive_meaning(name: &str) -> bool {
    if matches!(name, "timeout" | "deadline" | "duration" | "port" | "pid" | "fd") {
        return true;
    }
    const SUFFIXES: &[&str] = &[
        "_ns", "_us", "_ms", "_sec", "_secs", "_bytes", "_kib", "_mib", "_gib", "_hz", "_deg",
        "_rad", "_id", "_pid", "_fd", "_port",
    ];
    let mut idx = 0;
    while idx < SUFFIXES.len() {
        let suf = SUFFIXES[idx];
        if name.len() > suf.len() && name.ends_with(suf) {
            return true;
        }
        idx += 1;
    }
    false
}

/// Strips optional `b`/`c`/`r` prefix, raw `#` delimiters, and outer `"` quotes from a
/// string-like literal token, returning its inner text (or `None` for char literals, etc.).
#[inline]
fn unquote_string_literal(lit: &str) -> Option<&str> {
    let rest = lit
        .strip_prefix("br")
        .or_else(|| lit.strip_prefix("cr"))
        .or_else(|| lit.strip_prefix('r'))
        .map(|raw| raw.trim_matches('#'))
        .or_else(|| lit.strip_prefix('b'))
        .or_else(|| lit.strip_prefix('c'))
        .unwrap_or(lit);
    rest.strip_prefix('"')?.strip_suffix('"')
}

/// True when `inner` names the macOS `sandbox-exec` binary directly (bare or `/usr/bin/`).
#[inline]
fn is_sandbox_exec_program(inner: &str) -> bool {
    let base = inner.strip_prefix("/usr/bin/").unwrap_or(inner);
    base.len() == 12 && base.starts_with("sandbox-") && base.ends_with("exec")
}
