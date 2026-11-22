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
];

blaze_phf::name_phf_table!(pub(crate) static WORD = WORDS;);

/// A token that is no word of [`WORDS`].
pub(crate) const NO_WORD: u8 = u8::MAX;

const _: () = assert!(WORDS.len() < NO_WORD as usize, "word numbers fit a byte below NO_WORD");

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

const fn renames_in() -> bool {
    let mut at = 0;
    // Bounded: one turn per row of the rename table.
    while at < RENAMES.len() {
        word(RENAMES[at].old);
        at += 1;
    }
    true
}

const _: () = assert!(all_in(&crate::engine::COLLECTIONS) && all_in(&crate::engine::FORMAT_MACROS));
const _: () = assert!(all_in(&crate::engine::DEFINING) && all_in(&crate::engine::DISPATCHED) && renames_in());
const _: () = assert!(all_in(&crate::engine::STATIC_WORDS) && all_in(&crate::engine::SANS_IO_WORDS));
const _: () = assert!(all_in(&crate::engine::LOCK_TYPES));

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
/// Any other token, a word no rule reads, and a hole: no sequence has it, so it ends every match.
pub(crate) const OTHER: u8 = W + 9;
/// The size of the alphabet.
pub(crate) const SYMS: usize = OTHER as usize + 1;

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
use Cut::{DropFirst, DropLast, Inner, Whole};
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
