//! The words the rules read and the token sequences they look for, as tables fixed when the
//! crate compiles: the word table is a perfect hash (blaze-phf's, keyed by `name_hash`), so an
//! identifier's word number costs one probe while lexing; the sequences are one trie over
//! symbols ([`crate::matcher`]), which the pass advances once per token for every sequence rule
//! at once.
//!
//! A symbol is a word's number (its index in [`WORDS`]), or one of the punctuation symbols after
//! them. `::`, `..` and `!=` are one symbol each, so `. await` never matches `..await` and
//! `todo !` never matches `todo != x`. Whitespace and comments are not tokens, so a sequence
//! matches across them; a child item (a hole) is [`OTHER`], so no sequence matches across one.

use crate::matcher::{LIVE, Pattern, Trie, nodes_for};
use crate::{RENAMES, Template};

/// Every word a rule compares a token with: the words the pass dispatches on (`fn`, `impl`,
/// `unsafe`, ..), the rules' word tables (collections, formatting macros, renamed methods), and
/// the words of the sequences. A word not here is no rule's: the pass reads it only as a
/// possible recursive call (`Pass::on_word`). The checks below make leaving one out a compile
/// error.
pub(crate) const WORDS: &[&str] = &[
    // The pass's own dispatch (engine.rs `on_word`).
    "use", "as", "clone", "Global", "fn", "type", "impl", "trait", "mod", "for", "self", "unwrap", "expect", "panic",
    "unreachable", "todo", "unimplemented", "std", "alloc", "unsafe", "struct", "enum", "union",
    // Collections (`COLLECTIONS`), formatting macros (`FORMAT_MACROS`).
    "Vec", "Box", "String", "Rc", "Arc", "BTreeMap", "BTreeSet", "VecDeque", "BinaryHeap", "HashMap", "HashSet", "format",
    "format_args", "write", "writeln", "print", "println", "eprint", "eprintln",
    // Renamed methods (`RENAMES`).
    "insert", "remove", "swap_remove", "insert_many", "try_insert_many", "contains", "position",
    // The sequences.
    "Alloc", "global", "SharedAlloc", "Heap", "process", "testing", "heap", "new", "with_capacity", "from", "vec",
    "to_string", "to_owned", "to_vec", "async", "move", "await", "Instant", "now", "SystemTime", "env", "var", "vars",
    "args", "current_dir", "thread_rng", "RandomState", "default", "exit", "id", "dbg", "recv", "join", "wait", "lock",
    "loop", "collections", "hash_map", "hash", "hashbrown", "DefaultHasher", "new_in", "with_capacity_in",
    "from_iter_in", "IndexMap", "IndexSet", "ByHash", "ByHashSet",
    // `process-static` (engine.rs `STATIC_WORDS`) and `sans-io-core` (`SANS_IO_WORDS`), which
    // the pass dispatches on; their other words are above.
    "static", "thread_local", "fs", "net", "io", "os", "thread", "File", "TcpStream", "TcpListener", "UdpSocket", "libc",
    "rustix", "block_on",
    // `lock-without-reason` (engine.rs `LOCK_TYPES`): its constructors' words are compared as
    // text after the type and `::`, and `new` and `new_in` are above.
    "Lock", "ReadLock", "RwLock", "RankedLock", "SharedMap",
    // `workspace-unsafe` and `seqcst-unexplained`.
    "extern", "Ordering", "SeqCst",
    // `raw-sync`.
    "sync", "Mutex", "Condvar", "Barrier", "parking_lot", "dashmap", "DashMap", "mpsc", "channel", "sync_channel",
    // `blocking-on-pool`.
    "sleep", "spawn", "scope", "yield_now", "park", "available_parallelism", "hint", "spin_loop",
    // `transmute`.
    "mem", "transmute", "transmute_copy",
    // Strengthened `ambient-hasher` and `ambient-heap`.
    "rustc_hash", "FxHashMap", "FxHashSet", "ahash", "AHashMap", "AHashSet", "foldhash", "pin", "into_boxed_slice",
    "into_boxed_str",
    // `mut-ref` (`&mut` / `ref mut` -> `&uniq` / `ref uniq`) and `mut-for-uniq-borrow` (`let mut`).
    "mut", "ref", "let", "uniq",
    // `magic-slice-len` (`.get(0..3) == Some(..)`, `.get(at..at + 2)`, `.starts_with(..)` + advance).
    "get", "starts_with", "Some", "is_some_and", "try_into",
    // `manual-slice-idioms` (`.len() == 0`, `for i in 0..xs.len()`, `.find(..)` + slice) and `test-ambient-leak`.
    "len", "find", "in", "crate", "true", "false",
];

blaze_phf::name_phf_table!(pub(crate) static WORD = WORDS;);

/// A token that is no word of [`WORDS`].
pub(crate) const NO_WORD: u8 = u8::MAX;

const _: () = assert!(WORDS.len() < NO_WORD as usize, "word numbers fit a byte below NO_WORD");

/// Minimum and maximum byte lengths of any word in [`WORDS`], computed at compile time so the
/// lexer can skip the PHF probe for identifiers outside this range (e.g., all 1-byte names).
pub(crate) const MIN_WORD_LEN: usize = {
    let mut min = usize::MAX;
    let mut at = 0;
    while at < WORDS.len() {
        if WORDS[at].len() < min {
            min = WORDS[at].len();
        }
        at += 1;
    }
    min
};

pub(crate) const MAX_WORD_LEN: usize = {
    let mut max = 0;
    let mut at = 0;
    while at < WORDS.len() {
        if WORDS[at].len() > max {
            max = WORDS[at].len();
        }
        at += 1;
    }
    max
};

/// Per-length 64-bit mask of valid first ASCII bytes (`b - b'A'` in `0..64`, covering `'A'..='Z'`,
/// `'_'`, and `'a'..='z'`) across [`WORDS`]. Lets `Pass::lex_run` reject ~85% of identifiers in
/// two instructions before touching the PHF table.
pub(crate) const WORD_FIRST_BY_LEN: [u64; MAX_WORD_LEN + 1] = {
    let mut masks = [0u64; MAX_WORD_LEN + 1];
    let mut at = 0;
    while at < WORDS.len() {
        let b = WORDS[at].as_bytes();
        let bit = b[0].wrapping_sub(b'A');
        assert!(bit < 64, "every WORDS entry starts with ASCII letter or underscore");
        masks[b.len()] |= 1u64 << bit;
        at += 1;
    }
    masks
};

/// The number of `word` in [`WORDS`]. Panics, a compile error in a `const`, when it is not there.
pub(crate) const fn word(word: &str) -> u8 {
    let mut at = 0;
    // Bounded: one turn per word of the table.
    while at < WORDS.len() {
        if blaze_phf::bytes_eq(WORDS[at].as_bytes(), word.as_bytes()) {
            return at as u8;
        }
        at += 1;
    }
    panic!("a word the rules compare is missing from WORDS")
}

/// Bitmask classes for O(1) dispatch in `Pass::on_word` (`WORD_CLASS[t.word as usize]`).
pub(crate) const WC_DEFINING: u16 = 1 << 0;
pub(crate) const WC_COLLECTION: u16 = 1 << 1;
pub(crate) const WC_RENAME: u16 = 1 << 2;
pub(crate) const WC_STATIC: u16 = 1 << 3;
pub(crate) const WC_SANS_IO: u16 = 1 << 4;
pub(crate) const WC_LOCK_TYPE: u16 = 1 << 5;
pub(crate) const WC_DISPATCHED: u16 = 1 << 6;
pub(crate) const WC_IO_MODULE: u16 = 1 << 7;
pub(crate) const WC_IO_TYPE: u16 = 1 << 8;
pub(crate) const WC_PRINT_MACRO: u16 = 1 << 9;

/// Word indices in [`WORDS`] for O(1) integer dispatch without string comparisons.
pub(crate) const W_USE: u8 = word("use");
pub(crate) const W_AS: u8 = word("as");
pub(crate) const W_CLONE: u8 = word("clone");
pub(crate) const W_GLOBAL: u8 = word("Global");
pub(crate) const W_FN: u8 = word("fn");
pub(crate) const W_TYPE: u8 = word("type");
pub(crate) const W_IMPL: u8 = word("impl");
pub(crate) const W_TRAIT: u8 = word("trait");
pub(crate) const W_MOD: u8 = word("mod");
pub(crate) const W_FOR: u8 = word("for");
pub(crate) const W_SELF: u8 = word("self");
pub(crate) const W_UNWRAP: u8 = word("unwrap");
pub(crate) const W_EXPECT: u8 = word("expect");
pub(crate) const W_STD: u8 = word("std");
pub(crate) const W_ALLOC: u8 = word("alloc");
pub(crate) const W_UNSAFE: u8 = word("unsafe");
pub(crate) const W_EXTERN: u8 = word("extern");
pub(crate) const W_ORDERING: u8 = word("Ordering");
pub(crate) const W_SEQCST: u8 = word("SeqCst");
pub(crate) const W_STRING: u8 = word("String");
pub(crate) const W_VEC: u8 = word("Vec");
pub(crate) const W_ARC: u8 = word("Arc");
pub(crate) const W_THREAD_LOCAL: u8 = word("thread_local");
pub(crate) const W_FILE: u8 = word("File");
pub(crate) const W_RECV: u8 = word("recv");
pub(crate) const W_JOIN: u8 = word("join");
pub(crate) const W_LOCK: u8 = word("lock");
pub(crate) const W_WAIT: u8 = word("wait");
pub(crate) const W_BLOCK_ON: u8 = word("block_on");
pub(crate) const W_INSTANT: u8 = word("Instant");
pub(crate) const W_SYSTEM_TIME: u8 = word("SystemTime");
pub(crate) const W_LIBC: u8 = word("libc");
pub(crate) const W_RUSTIX: u8 = word("rustix");
pub(crate) const W_MUT: u8 = word("mut");
pub(crate) const W_REF: u8 = word("ref");
pub(crate) const W_LET: u8 = word("let");
pub(crate) const W_UNIQ: u8 = word("uniq");
pub(crate) const W_GET: u8 = word("get");
pub(crate) const W_STARTS_WITH: u8 = word("starts_with");
pub(crate) const W_SOME: u8 = word("Some");
pub(crate) const W_IS_SOME_AND: u8 = word("is_some_and");
pub(crate) const W_TRY_INTO: u8 = word("try_into");
pub(crate) const W_LEN: u8 = word("len");
pub(crate) const W_FIND: u8 = word("find");
pub(crate) const W_IN: u8 = word("in");
pub(crate) const W_CRATE: u8 = word("crate");
pub(crate) const W_TRUE: u8 = word("true");
pub(crate) const W_FALSE: u8 = word("false");
pub(crate) const W_ALLOC_TYPE: u8 = word("Alloc");
pub(crate) const W_GLOBAL_FN: u8 = word("global");

const fn mark_all(table: &mut [u16; WORDS.len()], words: &[&str], bit: u16) {
    let mut at = 0;
    // Bounded: one turn per word given.
    while at < words.len() {
        table[word(words[at]) as usize] |= bit;
        at += 1;
    }
}

/// Compile-time class bitmask for every word in [`WORDS`]. Also verifies that every word in the
/// engine's dispatch and rule tables is present in [`WORDS`].
pub(crate) const WORD_CLASS: [u16; WORDS.len()] = {
    let mut table = [0u16; WORDS.len()];
    mark_all(&mut table, &crate::engine::DEFINING, WC_DEFINING);
    mark_all(&mut table, &crate::engine::COLLECTIONS, WC_COLLECTION);
    mark_all(&mut table, &crate::engine::STATIC_WORDS, WC_STATIC);
    mark_all(&mut table, &crate::engine::SANS_IO_WORDS, WC_SANS_IO);
    mark_all(&mut table, &crate::engine::LOCK_TYPES, WC_LOCK_TYPE);
    mark_all(&mut table, &crate::engine::DISPATCHED, WC_DISPATCHED);
    mark_all(&mut table, &crate::engine::IO_MODULES, WC_IO_MODULE);
    mark_all(&mut table, &crate::engine::IO_TYPES, WC_IO_TYPE);
    mark_all(&mut table, &crate::engine::PRINT_MACROS, WC_PRINT_MACRO);
    let mut at = 0;
    while at < RENAMES.len() {
        table[word(RENAMES[at].old) as usize] |= WC_RENAME;
        at += 1;
    }
    table
};

/// Every word of `words` is one of [`WORDS`] (a compile error otherwise).
const fn all_in(words: &[&str]) -> bool {
    let mut at = 0;
    // Bounded: one turn per word given.
    while at < words.len() {
        word(words[at]);
        at += 1;
    }
    true
}

const _: () = assert!(all_in(&crate::engine::FORMAT_MACROS));

const W: u8 = WORDS.len() as u8;
/// `::`.
pub(crate) const SEP: u8 = W;
/// `.`, not part of `..`.
pub(crate) const DOT: u8 = W + 1;
/// `..` (and the first two dots of `...` or `..=`).
pub(crate) const DOTDOT: u8 = W + 2;
pub(crate) const OPEN: u8 = W + 3;
pub(crate) const CLOSE: u8 = W + 4;
/// `!`, not part of `!=`.
pub(crate) const BANG: u8 = W + 5;
/// `!=`.
pub(crate) const NE: u8 = W + 6;
pub(crate) const BRACE: u8 = W + 7;
pub(crate) const BAR: u8 = W + 8;
/// `<` (for turbofish `::<` sequences such as `mem::transmute::<`).
pub(crate) const LT: u8 = W + 9;
/// Any other token, a word no rule reads, and a hole: no sequence has it, so it ends every match.
pub(crate) const OTHER: u8 = W + 10;
/// The size of the alphabet.
pub(crate) const SYMS: usize = OTHER as usize + 1;

const _: () = assert!(SYMS <= u8::MAX as usize, "symbols fit in a u8");

/// One element of a sequence as it is written in the table.
#[derive(Clone, Copy, Debug)]
pub(crate) enum E {
    /// A word of [`WORDS`].
    W(&'static str),
    /// A punctuation symbol ([`SEP`], [`DOT`], [`OPEN`], ..).
    P(u8),
}

/// What must hold around a matched sequence besides its tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Guard {
    None,
    /// The first token is not a method's name (after a `.` that is not `..`), nor a function's
    /// being declared (after `fn`).
    NotMethod,
    /// `loop {` whose braces hold no `break`, `return` or `?` (child items left out).
    Unbounded,
}

/// Where a finding's range starts and ends within the matched tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cut {
    /// From the first token to the last.
    Whole,
    /// Without the last token (the `(` of a call, the `{` of a loop, the word after `async`).
    DropLast,
    /// Without the first token (a method's `.`).
    DropFirst,
    /// Without the first and the last (a method's `.` and its call's `(`).
    Inner,
    /// Without a trailing `::<` turbofish opener (three tokens: `:`, `:`, `<`).
    DropTurbo,
}

/// The argument a finding of a sequence carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArgOf {
    None,
    /// The row of [`SEQS`]: the message reads its `instead`.
    Row,
    /// The first token's range (a macro's name).
    FirstToken,
}

/// One sequence some rule looks for, and what its finding is.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seq {
    pub(crate) elems: &'static [E],
    /// The finding's template, which names its rule: a sequence of a rule not enabled is read
    /// past.
    pub(crate) template: Template,
    pub(crate) guard: Guard,
    pub(crate) cut: Cut,
    pub(crate) arg: ArgOf,
    /// What to write instead, set in the message after the matched text (`new_in(alloc)`).
    pub(crate) instead: &'static str,
}

const fn s(elems: &'static [E], template: Template, guard: Guard, cut: Cut, arg: ArgOf, instead: &'static str) -> Seq {
    Seq { elems, template, guard, cut, arg, instead }
}

use ArgOf::{FirstToken, Row};
use Cut::{DropFirst, DropLast, DropTurbo, Inner, Whole};
use E::{P, W as Wd};
use Guard::{NotMethod, Unbounded};

/// No guard: the tokens decide (`Guard::None`, named apart from `ArgOf::None`).
const fn none() -> Guard {
    Guard::None
}

/// Every sequence of every sequence rule. The order is the rows' numbers (an [`ArgOf::Row`]
/// argument), so a row is only ever added at the end; a change to a row's tokens raises its
/// rule's version.
pub(crate) const SEQS: &[Seq] = &[
    // ---- stops: the macros that stop the program (`.unwrap()`, `.expect(` and `[i]` stay in the pass).
    s(&[Wd("panic"), P(BANG)], Template::StopPanic, NotMethod, Whole, ArgOf::None, ""),
    s(&[Wd("unreachable"), P(BANG)], Template::StopUnreachable, NotMethod, Whole, ArgOf::None, ""),
    s(&[Wd("todo"), P(BANG)], Template::StopTodo, NotMethod, Whole, ArgOf::None, ""),
    s(&[Wd("unimplemented"), P(BANG)], Template::StopUnimplemented, NotMethod, Whole, ArgOf::None, ""),
    // ---- format-macro.
    s(&[Wd("format"), P(BANG)], Template::FormatMacro, NotMethod, Whole, FirstToken, ""),
    s(&[Wd("format_args"), P(BANG)], Template::FormatMacro, NotMethod, Whole, FirstToken, ""),
    s(&[Wd("write"), P(BANG)], Template::FormatMacro, NotMethod, Whole, FirstToken, ""),
    s(&[Wd("writeln"), P(BANG)], Template::FormatMacro, NotMethod, Whole, FirstToken, ""),
    s(&[Wd("print"), P(BANG)], Template::FormatMacro, NotMethod, Whole, FirstToken, ""),
    s(&[Wd("println"), P(BANG)], Template::FormatMacro, NotMethod, Whole, FirstToken, ""),
    s(&[Wd("eprint"), P(BANG)], Template::FormatMacro, NotMethod, Whole, FirstToken, ""),
    s(&[Wd("eprintln"), P(BANG)], Template::FormatMacro, NotMethod, Whole, FirstToken, ""),
    // ---- ambient-heap: the process's heap reached for.
    s(&[Wd("Alloc"), P(SEP), Wd("global"), P(OPEN)], Template::HeapReach, none(), DropLast, Row, "an `Alloc<'_>` parameter handed down from the entry"),
    s(&[Wd("SharedAlloc"), P(SEP), Wd("global"), P(OPEN)], Template::HeapReach, none(), DropLast, Row, "a `SharedAlloc<'_>` parameter handed down from the entry"),
    s(&[Wd("Heap"), P(SEP), Wd("process"), P(OPEN)], Template::HeapReach, none(), DropLast, Row, "the heap the entry made, passed down as an allocator"),
    s(&[Wd("testing"), P(SEP), Wd("heap"), P(OPEN)], Template::HeapReach, none(), DropLast, Row, "an allocator parameter; the testing heap is for test code"),
    // ---- ambient-heap: std's heap with no allocator named.
    s(&[Wd("Vec"), P(SEP), Wd("new"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`Vec::new_in(alloc)`"),
    s(&[Wd("Vec"), P(SEP), Wd("with_capacity"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`Vec::with_capacity_in(n, alloc)`"),
    s(&[Wd("Box"), P(SEP), Wd("new"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`Box::new_in(value, alloc)`"),
    s(&[Wd("String"), P(SEP), Wd("new"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`String::new_in(alloc)`"),
    s(&[Wd("String"), P(SEP), Wd("from"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`String::from_str_in(text, alloc)`"),
    s(&[Wd("Rc"), P(SEP), Wd("new"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`Rc::new_in(value, alloc)`"),
    s(&[Wd("Arc"), P(SEP), Wd("new"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`Arc::new_in(value, alloc)`"),
    s(&[Wd("vec"), P(BANG)], Template::HeapMacro, NotMethod, Whole, Row, "`vec_in![alloc; ..]`"),
    s(&[Wd("format"), P(BANG)], Template::HeapMacro, NotMethod, Whole, Row, "`format_in!(alloc, ..)`, or `format_into!` into the caller's buffer"),
    s(&[P(DOT), Wd("to_string"), P(OPEN)], Template::HeapMethod, none(), Inner, Row, "a string made in the caller's allocator (`format_in!`, `String::from_str_in`)"),
    s(&[P(DOT), Wd("to_owned"), P(OPEN)], Template::HeapMethod, none(), Inner, Row, "`to_owned_in(alloc)`"),
    s(&[P(DOT), Wd("to_vec"), P(OPEN)], Template::HeapMethod, none(), Inner, Row, "`to_owned_in(alloc)`, or `Vec::from_slice_in`-style copy into the caller's allocator"),
    // ---- no-async.
    s(&[Wd("async"), Wd("fn")], Template::Async, none(), DropLast, ArgOf::None, ""),
    s(&[Wd("async"), Wd("unsafe")], Template::Async, none(), DropLast, ArgOf::None, ""),
    s(&[Wd("async"), Wd("move")], Template::Async, none(), DropLast, ArgOf::None, ""),
    s(&[Wd("async"), P(BRACE)], Template::Async, none(), DropLast, ArgOf::None, ""),
    s(&[Wd("async"), P(BAR)], Template::Async, none(), DropLast, ArgOf::None, ""),
    s(&[P(DOT), Wd("await")], Template::Await, none(), DropFirst, ArgOf::None, ""),
    // ---- ambient-read.
    s(&[Wd("Instant"), P(SEP), Wd("now"), P(OPEN)], Template::ReadClock, none(), DropLast, Row, "a clock passed in"),
    s(&[Wd("SystemTime"), P(SEP), Wd("now"), P(OPEN)], Template::ReadClock, none(), DropLast, Row, "a clock passed in"),
    s(&[Wd("env"), P(SEP), Wd("var"), P(OPEN)], Template::ReadEnv, none(), DropLast, Row, "the environment read once at the entry and passed in"),
    s(&[Wd("env"), P(SEP), Wd("vars"), P(OPEN)], Template::ReadEnv, none(), DropLast, Row, "the environment read once at the entry and passed in"),
    s(&[Wd("env"), P(SEP), Wd("args"), P(OPEN)], Template::ReadEnv, none(), DropLast, Row, "the arguments parsed at the entry and passed in"),
    s(&[Wd("env"), P(SEP), Wd("current_dir"), P(OPEN)], Template::ReadEnv, none(), DropLast, Row, "a directory passed in"),
    s(&[Wd("thread_rng"), P(OPEN)], Template::ReadRandom, NotMethod, DropLast, Row, "a seeded generator passed in"),
    s(&[Wd("HashMap"), P(SEP), Wd("new"), P(OPEN)], Template::ReadHashSeed, none(), DropLast, Row, "a blaze_hash map with a seed passed in"),
    s(&[Wd("HashMap"), P(SEP), Wd("with_capacity"), P(OPEN)], Template::ReadHashSeed, none(), DropLast, Row, "a blaze_hash map with a seed passed in"),
    s(&[Wd("HashMap"), P(SEP), Wd("default"), P(OPEN)], Template::ReadHashSeed, none(), DropLast, Row, "a blaze_hash map with a seed passed in"),
    s(&[Wd("HashSet"), P(SEP), Wd("new"), P(OPEN)], Template::ReadHashSeed, none(), DropLast, Row, "a blaze_hash set with a seed passed in"),
    s(&[Wd("HashSet"), P(SEP), Wd("with_capacity"), P(OPEN)], Template::ReadHashSeed, none(), DropLast, Row, "a blaze_hash set with a seed passed in"),
    s(&[Wd("HashSet"), P(SEP), Wd("default"), P(OPEN)], Template::ReadHashSeed, none(), DropLast, Row, "a blaze_hash set with a seed passed in"),
    s(&[Wd("RandomState"), P(SEP), Wd("new"), P(OPEN)], Template::ReadHashSeed, none(), DropLast, Row, "a seed passed in"),
    s(&[Wd("RandomState"), P(SEP), Wd("default"), P(OPEN)], Template::ReadHashSeed, none(), DropLast, Row, "a seed passed in"),
    s(&[Wd("process"), P(SEP), Wd("exit"), P(OPEN)], Template::ReadProcess, none(), DropLast, Row, "an exit status returned to the entry"),
    s(&[Wd("process"), P(SEP), Wd("id"), P(OPEN)], Template::ReadProcess, none(), DropLast, Row, "the process's facts passed in from the entry"),
    // ---- left-undone.
    s(&[Wd("todo"), P(BANG)], Template::UndoneTodo, NotMethod, Whole, ArgOf::None, ""),
    s(&[Wd("unimplemented"), P(BANG)], Template::UndoneUnimplemented, NotMethod, Whole, ArgOf::None, ""),
    s(&[Wd("dbg"), P(BANG)], Template::UndoneDbg, NotMethod, Whole, ArgOf::None, ""),
    s(&[Wd("unreachable"), P(BANG), P(OPEN), P(CLOSE)], Template::UndoneUnreachable, NotMethod, Whole, ArgOf::None, ""),
    // ---- unbounded-wait.
    s(&[P(DOT), Wd("recv"), P(OPEN), P(CLOSE)], Template::WaitCall, none(), DropFirst, Row, "`recv_timeout` or a wait through `blaze_core::wait` with a deadline"),
    s(&[P(DOT), Wd("join"), P(OPEN), P(CLOSE)], Template::WaitCall, none(), DropFirst, Row, "a join with a deadline"),
    s(&[P(DOT), Wd("wait"), P(OPEN), P(CLOSE)], Template::WaitCall, none(), DropFirst, Row, "a wait with a deadline (`blaze_core::wait`)"),
    s(&[P(DOT), Wd("lock"), P(OPEN), P(CLOSE)], Template::WaitCall, none(), DropFirst, Row, "a named `blaze_core::lock` lock, whose wait has a deadline"),
    s(&[Wd("loop"), P(BRACE)], Template::WaitLoop, Unbounded, DropLast, ArgOf::None, ""),
    // ---- ambient-hasher (D125, D126): std's tables and hashers, whose hasher is not written.
    s(&[Wd("collections"), P(SEP), Wd("HashMap")], Template::HasherStd, none(), Whole, Row, "`blaze_hash::Map<K, V, FixedState, A>`, made by `Map::new(MapOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("collections"), P(SEP), Wd("HashSet")], Template::HasherStd, none(), Whole, Row, "`blaze_hash::Set<T, FixedState, A>`, made by `Set::new(SetOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("hash_map"), P(SEP), Wd("RandomState")], Template::HasherStd, none(), Whole, Row, "`FixedState`, or a `SipKey` from `Io::sip_key` for keys an outsider picks"),
    s(&[Wd("hash_map"), P(SEP), Wd("DefaultHasher")], Template::HasherStd, none(), Whole, Row, "`FixedState`, or a `SipKey` from `Io::sip_key` for keys an outsider picks"),
    s(&[Wd("hash"), P(SEP), Wd("RandomState")], Template::HasherStd, none(), Whole, Row, "`FixedState`, or a `SipKey` from `Io::sip_key` for keys an outsider picks"),
    s(&[Wd("hash"), P(SEP), Wd("DefaultHasher")], Template::HasherStd, none(), Whole, Row, "`FixedState`, or a `SipKey` from `Io::sip_key` for keys an outsider picks"),
    s(&[Wd("hashbrown"), P(SEP), Wd("HashMap")], Template::HasherStd, none(), Whole, Row, "`blaze_hash::Map<K, V, FixedState, A>`, made by `Map::new(MapOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("hashbrown"), P(SEP), Wd("HashSet")], Template::HasherStd, none(), Whole, Row, "`blaze_hash::Set<T, FixedState, A>`, made by `Set::new(SetOpts { hasher: FixedState, alloc, capacity: None })`"),
    // ---- ambient-hasher: the constructors that build the hasher from `Default`.
    s(&[Wd("HashMap"), P(SEP), Wd("new_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Map::new(MapOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("HashMap"), P(SEP), Wd("with_capacity_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(n) })`"),
    s(&[Wd("HashMap"), P(SEP), Wd("from_iter_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Map::new(MapOpts { hasher: FixedState, alloc, capacity: None })`, then `extend`"),
    s(&[Wd("HashSet"), P(SEP), Wd("new_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Set::new(SetOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("HashSet"), P(SEP), Wd("with_capacity_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Set::new(SetOpts { hasher: FixedState, alloc, capacity: Some(n) })`"),
    s(&[Wd("HashSet"), P(SEP), Wd("from_iter_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Set::new(SetOpts { hasher: FixedState, alloc, capacity: None })`, then `extend`"),
    s(&[Wd("IndexMap"), P(SEP), Wd("new_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`OrderMap::new(OrderMapOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("IndexMap"), P(SEP), Wd("with_capacity_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`OrderMap::new(OrderMapOpts { hasher: FixedState, alloc, capacity: Some(n) })`"),
    s(&[Wd("IndexMap"), P(SEP), Wd("from_iter_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`OrderMap::new(OrderMapOpts { hasher: FixedState, alloc, capacity: None })`, then `extend`"),
    s(&[Wd("IndexSet"), P(SEP), Wd("new_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`OrderSet::new(OrderSetOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("IndexSet"), P(SEP), Wd("with_capacity_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`OrderSet::new(OrderSetOpts { hasher: FixedState, alloc, capacity: Some(n) })`"),
    s(&[Wd("IndexSet"), P(SEP), Wd("from_iter_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`OrderSet::new(OrderSetOpts { hasher: FixedState, alloc, capacity: None })`, then `extend`"),
    s(&[Wd("ByHash"), P(SEP), Wd("new_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None })`"),
    s(&[Wd("ByHash"), P(SEP), Wd("with_capacity_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: Some(n) })`"),
    s(&[Wd("ByHash"), P(SEP), Wd("from_iter_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None })`, then `extend`"),
    s(&[Wd("ByHashSet"), P(SEP), Wd("new_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Set::new(SetOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None })`"),
    s(&[Wd("ByHashSet"), P(SEP), Wd("with_capacity_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Set::new(SetOpts { hasher: BuildKeyHasher::default(), alloc, capacity: Some(n) })`"),
    s(&[Wd("ByHashSet"), P(SEP), Wd("from_iter_in"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Set::new(SetOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None })`, then `extend`"),
    // ---- ambient-hasher (strengthened): third-party default-hasher maps/sets and bare constructors.
    s(&[Wd("rustc_hash"), P(SEP), Wd("FxHashMap")], Template::HasherStd, none(), Whole, Row, "`blaze_hash::Map<K, V, FixedState, A>`, made by `Map::new(MapOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("rustc_hash"), P(SEP), Wd("FxHashSet")], Template::HasherStd, none(), Whole, Row, "`blaze_hash::Set<T, FixedState, A>`, made by `Set::new(SetOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("ahash"), P(SEP), Wd("AHashMap")], Template::HasherStd, none(), Whole, Row, "`blaze_hash::Map<K, V, FixedState, A>`, made by `Map::new(MapOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("ahash"), P(SEP), Wd("AHashSet")], Template::HasherStd, none(), Whole, Row, "`blaze_hash::Set<T, FixedState, A>`, made by `Set::new(SetOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("foldhash"), P(SEP), Wd("HashMap")], Template::HasherStd, none(), Whole, Row, "`blaze_hash::Map<K, V, FixedState, A>`, made by `Map::new(MapOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("HashMap"), P(SEP), Wd("new"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Map::new(MapOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("HashSet"), P(SEP), Wd("new"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Set::new(SetOpts { hasher: FixedState, alloc, capacity: None })`"),
    s(&[Wd("HashMap"), P(SEP), Wd("with_capacity"), P(OPEN)], Template::HasherDefault, none(), DropLast, Row, "`Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(n) })`"),
    // ---- ambient-heap (strengthened): additional std constructors and allocating methods.
    s(&[Wd("BTreeMap"), P(SEP), Wd("new"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`BTreeMap::new_in(alloc)`"),
    s(&[Wd("BTreeSet"), P(SEP), Wd("new"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`BTreeSet::new_in(alloc)`"),
    s(&[Wd("VecDeque"), P(SEP), Wd("new"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`VecDeque::new_in(alloc)`"),
    s(&[Wd("VecDeque"), P(SEP), Wd("with_capacity"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`VecDeque::with_capacity_in(n, alloc)`"),
    s(&[Wd("BinaryHeap"), P(SEP), Wd("new"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`BinaryHeap::new_in(alloc)`"),
    s(&[Wd("String"), P(SEP), Wd("with_capacity"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`String::with_capacity_in(n, alloc)`"),
    s(&[Wd("Box"), P(SEP), Wd("pin"), P(OPEN)], Template::HeapConstructor, none(), DropLast, Row, "`Box::pin_in(value, alloc)`"),
    s(&[P(DOT), Wd("into_boxed_slice"), P(OPEN)], Template::HeapMethod, none(), Inner, Row, "a slice owned in the caller's allocator"),
    s(&[P(DOT), Wd("into_boxed_str"), P(OPEN)], Template::HeapMethod, none(), Inner, Row, "a string owned in the caller's allocator"),
    // ---- raw-sync: std / parking_lot / dashmap / mpsc primitives.
    s(&[Wd("sync"), P(SEP), Wd("Mutex")], Template::RawSync, none(), Whole, Row, "a named `blaze_core::lock::Lock` (or a lock-free D179 shape)"),
    s(&[Wd("sync"), P(SEP), Wd("RwLock")], Template::RawSync, none(), Whole, Row, "a named `blaze_core::lock::ReadLock` (or a lock-free D179 shape)"),
    s(&[Wd("sync"), P(SEP), Wd("Condvar")], Template::RawSync, none(), Whole, Row, "a deadline-bounded wait (`blaze_core::wait`)"),
    s(&[Wd("sync"), P(SEP), Wd("Barrier")], Template::RawSync, none(), Whole, Row, "structured worker coordination through the scheduler"),
    s(&[Wd("parking_lot"), P(SEP), Wd("Mutex")], Template::RawSync, none(), Whole, Row, "a named `blaze_core::lock::Lock` (or a lock-free D179 shape)"),
    s(&[Wd("parking_lot"), P(SEP), Wd("RwLock")], Template::RawSync, none(), Whole, Row, "a named `blaze_core::lock::ReadLock` (or a lock-free D179 shape)"),
    s(&[Wd("parking_lot"), P(SEP), Wd("Condvar")], Template::RawSync, none(), Whole, Row, "a deadline-bounded wait (`blaze_core::wait`)"),
    s(&[Wd("dashmap"), P(SEP), Wd("DashMap")], Template::RawSync, none(), Whole, Row, "partitioned ownership or a named `SharedMap` with a D179 reason"),
    s(&[Wd("mpsc"), P(SEP), Wd("channel"), P(OPEN)], Template::RawSync, none(), DropLast, Row, "a bounded channel (`blaze_core::chan::bounded`)"),
    s(&[Wd("mpsc"), P(SEP), Wd("sync_channel"), P(OPEN)], Template::RawSync, none(), DropLast, Row, "a bounded channel (`blaze_core::chan::bounded`)"),
    // ---- blocking-on-pool: OS thread blocking / spawning / spinning on a pool.
    s(&[Wd("thread"), P(SEP), Wd("sleep"), P(OPEN)], Template::BlockingOnPool, none(), DropLast, Row, "a deadline or timer on the driver rather than blocking a pool thread"),
    s(&[Wd("thread"), P(SEP), Wd("spawn"), P(OPEN)], Template::BlockingOnPool, none(), DropLast, Row, "work submitted to the scheduler pool rather than spawning an OS thread"),
    s(&[Wd("thread"), P(SEP), Wd("scope"), P(OPEN)], Template::BlockingOnPool, none(), DropLast, Row, "structured work on the scheduler pool rather than OS-thread scoping"),
    s(&[Wd("thread"), P(SEP), Wd("yield_now"), P(OPEN)], Template::BlockingOnPool, none(), DropLast, Row, "returning to the scheduler rather than yielding the OS thread"),
    s(&[Wd("thread"), P(SEP), Wd("park"), P(OPEN)], Template::BlockingOnPool, none(), DropLast, Row, "a bounded wait through the scheduler rather than parking the OS thread"),
    s(&[Wd("thread"), P(SEP), Wd("available_parallelism"), P(OPEN)], Template::BlockingOnPool, none(), DropLast, Row, "the pool's configured worker count passed in"),
    s(&[Wd("hint"), P(SEP), Wd("spin_loop"), P(OPEN)], Template::BlockingOnPool, none(), DropLast, Row, "a bounded wait rather than busy-spinning on a pool thread"),
    // ---- transmute: raw bit reinterpretation.
    s(&[Wd("mem"), P(SEP), Wd("transmute"), P(OPEN)], Template::Transmute, none(), DropLast, Row, "a safe `bstd::pod` / `blaze_own::pod` conversion (`pod::cast`, `pod::cast_slice`, `pod::bytes_of`, `pod::from_bytes`, `CheckedBitPattern`) or `from_ne_bytes`/`to_bits`"),
    s(&[Wd("mem"), P(SEP), Wd("transmute"), P(SEP), P(LT)], Template::Transmute, none(), DropTurbo, Row, "a safe `bstd::pod` / `blaze_own::pod` conversion (`pod::cast`, `pod::cast_slice`, `pod::bytes_of`, `pod::from_bytes`, `CheckedBitPattern`) or `from_ne_bytes`/`to_bits`"),
    s(&[Wd("mem"), P(SEP), Wd("transmute_copy"), P(OPEN)], Template::Transmute, none(), DropLast, Row, "a safe `bstd::pod` / `blaze_own::pod` conversion (`pod::pod_read_unaligned`, `pod::cast`, `CheckedBitPattern`) or `from_ne_bytes`"),
    s(&[Wd("mem"), P(SEP), Wd("transmute_copy"), P(SEP), P(LT)], Template::Transmute, none(), DropTurbo, Row, "a safe `bstd::pod` / `blaze_own::pod` conversion (`pod::pod_read_unaligned`, `pod::cast`, `CheckedBitPattern`) or `from_ne_bytes`"),
];

/// The symbols of `seq`.
const fn pattern(seq: &Seq) -> Pattern {
    assert!(!seq.elems.is_empty() && seq.elems.len() <= LIVE, "a sequence of 1 to LIVE symbols");
    let mut out = Pattern::EMPTY;
    let mut at = 0;
    // Bounded: one turn per element, LIVE at most.
    while at < seq.elems.len() {
        out.syms[at] = match seq.elems[at] {
            E::W(name) => word(name),
            E::P(sym) => {
                assert!(sym >= SEP && sym < OTHER, "a punctuation symbol");
                sym
            }
        };
        at += 1;
    }
    out.len = seq.elems.len() as u8;
    out
}

const N_SEQS: usize = SEQS.len();

/// [`SEQS`] as symbols, row for row.
pub(crate) const PATTERNS: [Pattern; N_SEQS] = {
    let mut out = [Pattern::EMPTY; N_SEQS];
    let mut at = 0;
    // Bounded: one turn per row.
    while at < N_SEQS {
        out[at] = pattern(&SEQS[at]);
        at += 1;
    }
    out
};

/// The nodes the trie uses, counted exactly ([`nodes_for`]; `Trie::build` checks every row is
/// used), so the table has no unused rows and is built once.
const N_NODES: usize = nodes_for(&PATTERNS);

/// The one trie of every sequence, built as the crate compiles.
pub(crate) static TRIE: Trie<N_NODES, SYMS, N_SEQS> = Trie::build(&PATTERNS);
