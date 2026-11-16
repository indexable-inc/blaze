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

use crate::{
    Arg, Block, Finding, Findings, Fix, LintError, Linted, Mark, ONLY_OWN, Owner, Rule, RuleSet, Run, Scope, Span,
    Template, TestBits, Unit, Walk, count,
};

/// A token's kind, as the rules tell kinds apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum K {
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
struct Tok {
    start: u32,
    end: u32,
    k: K,
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
enum Plan {
    None,
    /// The method name at `name` becomes row `row`'s new name, and when the row has a suffix,
    /// it goes at `after` (the call's `)` end).
    Rename { row: u32, name: Span, after: u32 },
}

/// A finding before test code is known.
#[derive(Clone, Copy)]
struct Cand {
    template: Template,
    range: Span,
    arg: Option<Arg>,
    plan: Plan,
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
const HANDLES: [&str; 2] = ["Arc", "Rc"];
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
const COLLECTIONS: [&str; 11] =
    ["Vec", "Box", "String", "Rc", "Arc", "BTreeMap", "BTreeSet", "VecDeque", "BinaryHeap", "HashMap", "HashSet"];
const DEFINING: [&str; 5] = ["struct", "enum", "union", "trait", "type"];
const FORMAT_MACROS: [&str; 8] = ["format", "format_args", "write", "writeln", "print", "println", "eprint", "eprintln"];

pub(crate) fn run<'a>(rules: &RuleSet, unit: &Unit<'_>, alloc: Alloc<'a>, out: &mut Findings<'a>) -> Result<Linted<'a>, LintError> {
    if u32::try_from(unit.text.len()).is_err() {
        return Err(LintError::TooLong { len: unit.text.len() });
    }
    let mut pass = Pass {
        rules,
        text: unit.text,
        bytes: unit.text.as_bytes(),
        scope: unit.scope,
        // Sized by the unit's own bytes (about one token per five), not its children's.
        toks: AVec::with_capacity_in(own_bytes(unit) / 5 + 8, alloc),
        cands: AVec::new_in(alloc),
        regions: AVec::new_in(alloc),
        frames: AVec::new_in(alloc),
        bodies: AVec::new_in(alloc),
        fns: AVec::new_in(alloc),
        children: AVec::with_capacity_in(unit.holes.len(), alloc),
        header: None,
        sig: None,
        in_use: false,
        defining: false,
        last_fn: None,
    };
    pass.lex(unit.holes)?;
    pass.walk();
    let test_at_end = pass.test_at(unit.text.len().saturating_sub(1) as u32);
    let from = count(out.list.len())?;
    pass.emit(out)?;
    let to = count(out.list.len())?;
    Ok(Linted { findings: Run { from, to }, children: pass.children, test_at_end })
}

/// The bytes of `unit` outside its holes (holes not yet checked: a bad one only mis-sizes).
fn own_bytes(unit: &Unit<'_>) -> usize {
    let holes: usize = unit.holes.iter().map(|h| h.end.saturating_sub(h.start) as usize).sum();
    unit.text.len().saturating_sub(holes)
}

struct Pass<'u, 'a> {
    rules: &'u RuleSet,
    text: &'u str,
    bytes: &'u [u8],
    scope: Scope,
    toks: AVec<'a, Tok>,
    cands: AVec<'a, Cand>,
    /// Test code: each region's range and which test code it is.
    regions: AVec<'a, (Span, TestBits)>,
    frames: AVec<'a, Frame>,
    /// The functions whose bodies are open, innermost last (indices of `fns`).
    bodies: AVec<'a, u32>,
    fns: AVec<'a, FnRec>,
    children: AVec<'a, Scope>,
    /// From `impl`, `trait` or `mod` to the `{` or `;` after it: the block it opens.
    header: Option<Block>,
    sig: Option<Sig>,
    /// From the word `use` to its `;`.
    in_use: bool,
    /// The word before was `struct`, `enum`, `union`, `trait` or `type`.
    defining: bool,
    /// The last `fn` read: where a local's declaration is looked for from.
    last_fn: Option<usize>,
}

impl<'u, 'a> Pass<'u, 'a> {
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
            self.toks.push(Tok { start: hole.start, end: hole.end, k: K::Hole });
            at = end;
        }
        self.lex_run(at, len);
        Ok(())
    }

    fn lex_run(&mut self, from: usize, to: usize) {
        let mut pos = from as u32;
        for token in tokenize(&self.text[from..to], FrontmatterAllowed::No, Cpu::BASELINE) {
            let start = pos;
            let end = pos + token.len;
            pos = end;
            let k = match token.kind {
                TokenKind::Whitespace | TokenKind::BlockComment { .. } => continue,
                TokenKind::LineComment { .. } => {
                    if self.rules.runs(Rule::Allocs) {
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
            self.toks.push(Tok { start, end, k });
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

    fn test_at(&self, pos: u32) -> TestBits {
        let mut bits = self.scope.test;
        for (span, flavor) in self.regions.iter() {
            if span.start <= pos && pos < span.end {
                bits = bits.union(*flavor);
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
                        self.index(i);
                    }
                }
                K::P(b')' | b']') => {
                    if let Some(sig) = &mut self.sig {
                        sig.depth = sig.depth.saturating_sub(1);
                    }
                }
                K::P(b'#') => self.attribute(i, t),
                K::Ident => self.on_word(i, t),
                K::Raw => self.defining = false,
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
            recursion_above: self.recursion_mark_above(line, parent.recursion_above),
        };
        let scope = Scope {
            test: self.test_at(t.start),
            block: self.innermost(),
            assoc: self.assoc_now(),
            in_module: self.in_module(),
            bin_root: self.scope.bin_root,
            build_script: self.scope.build_script,
            context,
        };
        self.children.push(scope);
    }

    fn on_word(&mut self, i: usize, t: Tok) {
        let w = self.str_of(t);
        let was_defining = core::mem::replace(&mut self.defining, DEFINING.contains(&w));
        let rules = self.rules;
        match w {
            "use" => self.in_use = true,
            "as" if rules.runs(Rule::Casts) => self.cast(i, t),
            "clone" if rules.runs(Rule::Clones) => self.clone_call(i, t),
            "Global" if rules.runs(Rule::Allocs) && !self.in_use => {
                self.push(Template::AllocGlobal, t.start, t.end, None);
            }
            "fn" => self.fn_word(i, t),
            "type" => self.type_word(i, t),
            "impl" | "trait" | "mod" if self.sig.is_none() => {
                self.header = Some(match w {
                    "impl" => Block::Impl,
                    "trait" => Block::Trait,
                    _ => Block::Module,
                });
            }
            "for" if self.header == Some(Block::Impl) && !matches!(self.tok(i + 1), Some(n) if n.k == K::P(b'<')) => {
                self.header = Some(Block::TraitImpl);
            }
            "self" => {
                if let Some(sig) = self.sig
                    && sig.depth == 1
                {
                    self.fns[sig.item as usize].has_self = true;
                }
            }
            "unwrap" | "expect" | "panic" | "unreachable" | "todo" | "unimplemented" if rules.runs(Rule::Stops) => {
                self.stop(i, t, w);
            }
            "std" | "alloc" if rules.runs(Rule::Ambient) => self.ambient_head(i, t, w),
            "unsafe" if rules.runs(Rule::UnsafeComment) => self.unsafe_word(i, t),
            _ => {}
        }
        if rules.runs(Rule::Ambient) && COLLECTIONS.contains(&w) && !self.in_use && !was_defining {
            self.collection(i, t, w);
        }
        if rules.runs(Rule::FormatMacro) && FORMAT_MACROS.contains(&w) {
            self.format_macro(i, t);
        }
        if rules.runs(Rule::Renamed) && rules.renames().iter().any(|r| r.old == w) {
            self.renamed(i, t, w);
        }
        if rules.runs(Rule::Recursion) {
            self.recursion_name(i, t, w);
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
            self.regions.push((Span::new(t.start, self.text.len() as u32), TestBits::CFG));
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
        self.regions.push((Span::new(t.start, region_end), flavor));
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
        let context = self.scope.context;
        let template = match self.look(t.start as usize, clone_mark, context.clone_above, context.clone_after) {
            Some(Mark::Handle) => Template::CloneHandle,
            Some(Mark::ErrorPath) => Template::CloneErrorPath,
            Some(Mark::Deep) => Template::CloneDeep,
            _ => {
                let handle = match receiver {
                    // A field's declaration is the file's, which an item does not see: its own
                    // text is read (clones.rs reads the whole file).
                    Some((name, true)) => self.declared_handle(0, self.toks.len(), name, false),
                    Some((name, false)) => self.declared_handle(self.last_fn.unwrap_or(0), i, name, true),
                    None => false,
                };
                if handle { Template::CloneHandle } else { Template::CloneUnknown }
            }
        };
        let end = if self.method(i) { self.tok(i + 2).map_or(t.end, |c| c.end) } else { t.end };
        self.push(template, t.start, end, None);
    }

    /// clones.rs's `receiver_of`: the last name of the run of names and dots before the `.`
    /// at `dot`, and whether it is a field (a dot in the run, or a `)` or `]` before it).
    fn receiver(&self, dot: usize) -> Option<(&'u str, bool)> {
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
        let field = run_start < dot - 1 || matches!(self.back(run_start, 1), Some(b) if matches!(b.k, K::P(b')' | b']')));
        Some((name, field))
    }

    /// clones.rs's `declared_handle` over tokens `from..to`: `name: [&][mut] path::Arc<` or
    /// `Rc<`, and with `assigned`, `name = path::Arc::..` or `Rc::..`.
    fn declared_handle(&self, from: usize, to: usize, name: &str, assigned: bool) -> bool {
        let mut k = from;
        // Bounded: one turn per token of the range.
        while k < to {
            if self.is_word(self.tok(k), name) {
                let next = self.tok(k + 1);
                if matches!(next, Some(n) if n.k == K::P(b':')) && !self.colons_at(k + 1) {
                    if self.handle_type(k + 2) {
                        return true;
                    }
                } else if assigned
                    && let Some(eq) = next
                    && eq.k == K::P(b'=')
                    && !matches!(self.tok(k + 2), Some(n) if matches!(n.k, K::P(b'=' | b'>')) && n.start == eq.end)
                    && self.handle_value(k + 2)
                {
                    return true;
                }
            }
            k += 1;
        }
        false
    }

    fn handle_type(&self, mut k: usize) -> bool {
        // Bounded: each turn moves `k` forward past one `&`.
        while matches!(self.tok(k), Some(t) if t.k == K::P(b'&')) {
            k += 1;
        }
        if self.is_word(self.tok(k), "mut") {
            k += 1;
        }
        let Some(last) = self.path_end(k) else { return false };
        HANDLES.contains(&self.word(self.tok(last)).unwrap_or("")) && matches!(self.tok(last + 1), Some(t) if t.k == K::P(b'<'))
    }

    fn handle_value(&self, k: usize) -> bool {
        let Some(last) = self.path_end(k) else { return false };
        // `Arc::<T>::default()`: the path stops at a turbofish, its last word the type (as
        // clones.rs's `path_run` stops at the `<`).
        if self.colons_at(last + 1) {
            return HANDLES.contains(&self.word(self.tok(last)).unwrap_or(""));
        }
        last >= k + 3 && HANDLES.contains(&self.word(self.tok(last - 3)).unwrap_or(""))
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

    /// The `<..>` opening at `open` (to `limit`): its count of top-level arguments (a trailing
    /// comma adds none), its first and its last argument as token ranges.
    fn type_args(&self, open: usize, limit: usize) -> Option<(usize, (usize, usize), (usize, usize))> {
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
                        return Some((count, first.unwrap_or(last), last));
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
                    && let Some((count, _, last)) = self.type_args(k + 1, to)
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
                                self.type_args(k + 4, to).is_some_and(|(count, _, _)| count == 1)
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
                    if let Some((count, _, last)) = self.type_args(k + 1, to)
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
                let (_, (a, b), _) = self.type_args(path_to, to)?;
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
    fn recursion_name(&mut self, i: usize, t: Tok, w: &str) {
        if self.sig.is_some() {
            return;
        }
        let Some(&inner) = self.bodies.last() else { return };
        let f = self.fns[inner as usize];
        if f.recursive || f.name.of(self.text) != w {
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

    /// stops.rs: `.unwrap()`, `.expect(`, and `panic!`, `unreachable!`, `todo!`,
    /// `unimplemented!` (not after `.`, not `!=`).
    fn stop(&mut self, i: usize, t: Tok, w: &str) {
        let next = self.tok(i + 1);
        let method = self.method(i);
        match w {
            "unwrap" if method && matches!(next, Some(n) if n.k == K::P(b'(')) => {
                if let Some(close) = self.tok(i + 2)
                    && close.k == K::P(b')')
                {
                    self.push(Template::StopUnwrap, t.start, close.end, None);
                }
            }
            "expect" if method && matches!(next, Some(n) if n.k == K::P(b'(')) => {
                self.push(Template::StopExpect, t.start, t.end, None);
            }
            "panic" | "unreachable" | "todo" | "unimplemented" if !method => {
                let Some(bang) = next else { return };
                if bang.k != K::P(b'!') {
                    return;
                }
                if matches!(self.tok(i + 2), Some(e) if e.k == K::P(b'=') && e.start == bang.end) {
                    return;
                }
                let template = match w {
                    "panic" => Template::StopPanic,
                    "unreachable" => Template::StopUnreachable,
                    "todo" => Template::StopTodo,
                    _ => Template::StopUnimplemented,
                };
                self.push(template, t.start, bang.end, None);
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
    fn ambient_head(&mut self, i: usize, t: Tok, w: &str) {
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
        let template = if w == "std" { Template::AmbientStd } else { Template::AmbientAlloc };
        let arg = if path {
            self.tok(i + 3).filter(|s| s.k == K::Ident).map(|s| Arg::Span(Span::new(s.start, s.end)))
        } else {
            None
        };
        self.push(template, t.start, t.end, arg);
    }

    /// ambient.rs's `vec_box` for a collection named through `std::` or `alloc::` (the bare
    /// names wait for the file's imports: a rule of the second kind, not built).
    fn collection(&mut self, i: usize, t: Tok, w: &str) {
        let shaped =
            w == "String" || matches!(self.tok(i + 1), Some(n) if n.k == K::P(b'<')) || self.colons_at(i + 1);
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
        if matches!(self.str_of(first), "std" | "alloc") {
            self.push(Template::AmbientCollection, first.start, t.end, Some(Arg::Span(Span::new(first.start, first.end))));
        }
    }

    // ---- unsafe -------------------------------------------------------------------------

    /// An `unsafe` block or impl with no `// SAFETY:` comment on its line or above it (the
    /// clones walk: comment lines and a statement's unended lines are passed over).
    fn unsafe_word(&mut self, i: usize, t: Tok) {
        let next = self.tok(i + 1);
        let opens = matches!(next, Some(n) if n.k == K::P(b'{')) || self.is_word(next, "impl");
        if !opens {
            return;
        }
        let context = self.scope.context;
        let after = context.safety_after.then_some(Mark::Safety);
        if self.look(t.start as usize, safety_mark, context.safety_above, after).is_none() {
            self.push(Template::UnsafeNoComment, t.start, t.end, None);
        }
    }

    // ---- formatting macros --------------------------------------------------------------

    fn format_macro(&mut self, i: usize, t: Tok) {
        if self.method(i) {
            return;
        }
        let Some(bang) = self.tok(i + 1) else { return };
        if bang.k != K::P(b'!') || matches!(self.tok(i + 2), Some(e) if e.k == K::P(b'=') && e.start == bang.end) {
            return;
        }
        // `macro_rules! format` defines one: the name is not followed by `!`, so it is not here.
        self.push(Template::FormatMacro, t.start, bang.end, Some(Arg::Span(Span::new(t.start, t.end))));
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
            if let Plan::Rename { row, name, after } = c.plan {
                let rename = renames[row as usize];
                let edits_from = count(out.edits.len())?;
                out.push_edit(name, rename.new)?;
                if after != u32::MAX {
                    out.push_edit(Span::new(after, after), rename.suffix)?;
                }
                out.fixes.push(Fix { edits: Run { from: edits_from, to: count(out.edits.len())? } });
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
