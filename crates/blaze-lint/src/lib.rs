//! blaze's lints, rebuilt for the item layer (docs/v2/plan/design/lint.md).
//!
//! A rule is a pure function of one item: the item's own bytes (its text, with its child items'
//! ranges left out as holes) and the [`Scope`] its parent's text gives it. Every enabled rule
//! runs in one pass over the item's tokens ([`lint`]): the bytes are lexed once, and each token
//! goes to the rules registered for its kind (a word to the rules that read that word, a `[` to
//! the index rule, a line comment to the hatch rule), so the rules together cost about one lex.
//!
//! A [`Finding`] holds what rule 81 asks of a record and nothing else: its template (which names
//! its [`Rule`], a stable small number with a name and a version), an item-relative byte range
//! (never `file:line:col`, rule 71), its message as integer arguments (a range of the item, or a
//! row of the rule set's tables), and zero or more [`Fix`]es, each a list of [`Edit`]s. It has
//! no level: whether it is an error, advice or off is decided when it is printed, from the
//! [`Levels`] the configuration gives ([`Levels::of`]).
//!
//! What the results are a function of is a [`RuleSet`]'s identity ([`RuleSet::id`], a digest of
//! the rules' ids, versions and tables), the item's text and holes, and the scope's
//! [`Scope::key`]: the three go into the record's key (rule 65, computed before the work).
//!
//! # The floor (rule 67)
//!
//! A lint pass must read every byte of the item's own text once: the floor is one lex of those
//! bytes, plus one table probe per identifier. Each identifier gets its word number while it is
//! lexed, one probe of a perfect hash fixed at compile time (`sequences::WORDS`); a word no rule
//! reads costs nothing more. Every rule that looks for a token sequence (`todo !`, `. await`,
//! `Vec :: new (`) shares one trie built at compile time (`matcher`): per token, one load for
//! the root and one per live state (at most the longest sequence's length, usually none), the
//! same for one sequence rule or fifty. The other rules keep their dispatch on the word, with
//! short forward scans at a handful of words (`fn`, `type`, `[`, a test attribute, `loop {`). The example (`examples/run.rs`) prints the lex
//! of the same bytes beside the lint, and their ratio. Measured 2026-11-16 on the integration
//! tree's `crates/` (4,767 files, 89 MB, 290,858 units, one thread): lint 492 ms, lex 161 ms, a
//! ratio of 3.05; with no rule enabled 379 ms. So the distance to the floor is per unit (scratch
//! allocated per unit, a lexer start per run between holes), not the rules; scratch reused
//! across units is the next step.
//!
//! # What a typed rule will take
//!
//! Every rule here reads text and tokens only. A rule that needs types (a clone of a type that is
//! `Copy`, a receiver's real type for a rename) is the second kind: it takes the item's typed
//! body record as well as its text, and its key adds that record's key. [`Input`] names the two
//! kinds; only [`Input::Text`] is built.

#![no_std]
#![feature(allocator_api, variant_count)]

#[cfg(test)]
extern crate std;

mod engine;
mod fix;
mod matcher;
mod message;
mod sequences;
mod units;
mod variants;

#[cfg(test)]
mod tests;

use blaze_own::{AVec, Alloc};

pub use fix::{FixError, apply, apply_all};
pub use message::show_message;
pub use units::{UnitLinted, UnitPlan, Units};
pub use variants::Variants;

/// A byte range of an item's text, relative to the item's first byte (rule 71).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    #[inline]
    pub const fn new(start: u32, end: u32) -> Span {
        Span { start, end }
    }

    /// The text of `self` in `text`, which it is a range of.
    #[inline]
    pub fn of<'t>(self, text: &'t str) -> &'t str {
        &text[self.start as usize..self.end as usize]
    }

    #[inline]
    pub const fn len(self) -> u32 {
        self.end - self.start
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// A run of entries of one of [`Findings`]' columns, from `from` up to `to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Run {
    pub from: u32,
    pub to: u32,
}

impl Run {
    #[inline]
    pub const fn len(self) -> usize {
        (self.to - self.from) as usize
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.from == self.to
    }

    #[inline]
    fn range(self) -> core::ops::Range<usize> {
        self.from as usize..self.to as usize
    }
}

/// A lint rule: its number is its stable id (cited by records and configuration), its name the
/// one people write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Rule {
    /// A bare `as` cast to a primitive integer type (casts.rs).
    Casts = 1,
    /// A `.clone()` or `Clone::clone(..)` call, by what it duplicates (clones.rs).
    Clones = 2,
    /// What is left of the global heap: `Global`, a signature that fixes its caller's allocator,
    /// an `// alloc:` hatch, an expected `blaze::implicit_allocator` (allocs.rs).
    Allocs = 3,
    /// A function signature or alias whose error is untyped (untyped.rs).
    Untyped = 4,
    /// A function that calls itself directly (recursion.rs).
    Recursion = 5,
    /// A place a library can stop the program (stops.rs).
    Stops = 6,
    /// A path from `std` or `alloc` (ambient.rs).
    Ambient = 7,
    /// An `unsafe` block or impl with no comment stating its assumption above it.
    UnsafeComment = 8,
    /// A formatting macro (`format!`, `write!`, `println!` and family) outside test code.
    FormatMacro = 9,
    /// A name that moved: an old method name of blaze-own's collections.
    Renamed = 10,
    /// A heap reached for instead of handed down: the process's allocator (`Alloc::global()`,
    /// `Heap::process()`), or std's heap with no allocator named (`Vec::new()`, `format!`,
    /// `.to_string()`).
    AmbientHeap = 11,
    /// `async` code: blaze's own code is explicit state machines (`async fn`, `async {`,
    /// `.await`).
    NoAsync = 12,
    /// A read of process state not passed in (rule 75): the clock, the environment, randomness,
    /// a hash seed, the process itself.
    AmbientRead = 13,
    /// Work left undone: `todo!`, `unimplemented!`, `dbg!`, and `unreachable!()` with no message.
    LeftUndone = 14,
    /// A wait with no deadline (rule 18): `.recv()`, `.join()`, `.wait()`, `.lock()` with no
    /// argument, and a `loop` with no `break`, `return` or `?`.
    UnboundedWait = 15,
    /// A hash table whose hasher is not written at the use (decision D125): std's or hashbrown's
    /// tables and hashers (`std::collections::HashMap`, `hash_map::RandomState`,
    /// `DefaultHasher`), and the constructors that build the hasher from `Default`
    /// (`HashMap::new_in(`, `IndexSet::with_capacity_in(`).
    AmbientHasher = 16,
    /// An integer literal equal to the number of variants of an enum of its file, where a count
    /// is meant (a `const`'s whole value, an array's length, a range's end): written
    /// `core::mem::variant_count::<E>()` it cannot go stale (variants.rs). The one rule that
    /// reads its file, through [`Variants`].
    VariantCount = 17,
    /// An options struct (`#[blaze::args]`, rule 83, D126) written other than as a literal
    /// directly in the call that takes it: a TYPED rule, decided by the body check (blaze-typeck
    /// rules/args_inline.rs), never by this crate's token pass ([`Rule::typed`]). On from
    /// [`Step::Edition`] (advice), an error in [`Step::Strict`] ([`Step::levels`]).
    ArgsInline = 18,
}

impl Rule {
    /// How many rules there are: the largest id.
    pub const COUNT: usize = core::mem::variant_count::<Rule>();

    /// Every rule, by id.
    pub const ALL: [Rule; Rule::COUNT] = [
        Rule::Casts,
        Rule::Clones,
        Rule::Allocs,
        Rule::Untyped,
        Rule::Recursion,
        Rule::Stops,
        Rule::Ambient,
        Rule::UnsafeComment,
        Rule::FormatMacro,
        Rule::Renamed,
        Rule::AmbientHeap,
        Rule::NoAsync,
        Rule::AmbientRead,
        Rule::LeftUndone,
        Rule::UnboundedWait,
        Rule::AmbientHasher,
        Rule::VariantCount,
        Rule::ArgsInline,
    ];

    /// The ten rules ported from blaze's scripts: what [`Step::Strict`] runs. The rules after
    /// them run only where a workspace names them ([`Rule::from_name`], [`RuleSet::with`]).
    pub const HOUSE: [Rule; 10] = [
        Rule::Casts,
        Rule::Clones,
        Rule::Allocs,
        Rule::Untyped,
        Rule::Recursion,
        Rule::Stops,
        Rule::Ambient,
        Rule::UnsafeComment,
        Rule::FormatMacro,
        Rule::Renamed,
    ];

    #[inline]
    pub const fn id(self) -> u16 {
        self as u16
    }

    pub const fn name(self) -> &'static str {
        match self {
            Rule::Casts => "casts",
            Rule::Clones => "clones",
            Rule::Allocs => "allocs",
            Rule::Untyped => "untyped",
            Rule::Recursion => "recursion",
            Rule::Stops => "stops",
            Rule::Ambient => "ambient",
            Rule::UnsafeComment => "unsafe-comment",
            Rule::FormatMacro => "format-macro",
            Rule::Renamed => "renamed",
            Rule::AmbientHeap => "ambient-heap",
            Rule::NoAsync => "no-async",
            Rule::AmbientRead => "ambient-read",
            Rule::LeftUndone => "left-undone",
            Rule::UnboundedWait => "unbounded-wait",
            Rule::AmbientHasher => "ambient-hasher",
            Rule::VariantCount => "variant-count",
            Rule::ArgsInline => "args-inline",
        }
    }

    /// Whether the rule is typed: decided by the body check from types (its findings are the
    /// `BodyCheck` task's records, filed whatever the step, rule 81), never by the token pass.
    /// A typed rule is in no [`RuleSet`] a step makes, so a token record's key never moves with
    /// it; the step turns it on through its level alone ([`Step::levels`]).
    pub const fn typed(self) -> bool {
        matches!(self, Rule::ArgsInline)
    }

    /// The rule configuration names `name` ([`Rule::name`]'s spelling), if any.
    pub fn from_name(name: &str) -> Option<Rule> {
        Rule::ALL.into_iter().find(|rule| rule.name() == name)
    }

    /// The rule's version: raised whenever what it finds changes, so a record of the old one is
    /// not read as the new one's (the version joins [`RuleSet::id`]).
    pub const fn version(self) -> u16 {
        1
    }

    #[inline]
    const fn bit(self) -> u32 {
        1 << (self as u16)
    }

    /// The test code a site in exempts it from this rule: allocs.rs reads only `#[cfg(test)]`
    /// (and test files), every other rule test functions (`#[test]`, `#[x::test]`) too; an
    /// `unsafe` needs its comment in tests as anywhere, and a count goes stale in a test as
    /// anywhere.
    #[inline]
    pub const fn exempt(self) -> TestBits {
        match self {
            Rule::Allocs => TestBits::CFG,
            Rule::UnsafeComment | Rule::VariantCount => TestBits::NONE,
            _ => TestBits::ANY,
        }
    }
}

/// What a finding's message says: one variant per kind of finding, each of one [`Rule`]. Its
/// arguments are integers ([`Arg`]); the words are the template's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Template {
    /// `x as u32`; argument: the type's range.
    CastToInt = 1,
    /// A clone whose comment, or whose receiver's declaration, says it is a refcount's.
    CloneHandle = 2,
    /// A clone whose comment says it is on the error path.
    CloneErrorPath = 3,
    /// A clone whose comment says it copies an allocation.
    CloneDeep = 4,
    /// A clone nothing says the kind of.
    CloneUnknown = 5,
    /// The word `Global` outside a `use`.
    AllocGlobal = 10,
    /// A signature that fixes its caller's allocator; argument: the type's range.
    AllocSignature = 11,
    /// An `// alloc: <reason>` hatch.
    AllocComment = 12,
    /// `#[allow(..)]` or `#[expect(..)]` of `blaze::implicit_allocator`.
    AllocExpect = 13,
    /// `Result<_, String>` in a signature; argument: the error type's range.
    UntypedString = 20,
    /// `Result<_, &str>`; argument: the error type's range.
    UntypedStr = 21,
    /// `Result<_, Box<dyn Error>>`; argument: the error type's range.
    UntypedBoxed = 22,
    /// `anyhow::Error` or `anyhow::Result<T>`; argument: its range.
    UntypedAnyhow = 23,
    /// A `type` alias of such a `Result`; argument: the aliased type's range.
    UntypedAlias = 24,
    /// A function whose body calls it; argument: the call's range.
    Recursion = 30,
    StopUnwrap = 40,
    StopExpect = 41,
    StopPanic = 42,
    StopUnreachable = 43,
    StopTodo = 44,
    StopUnimplemented = 45,
    /// An index that can be out of bounds.
    StopIndex = 46,
    /// A path from `std`; argument: the segment after `std::`, when there is one.
    AmbientStd = 50,
    /// A path from `alloc`; argument: the segment after `alloc::`, when there is one.
    AmbientAlloc = 51,
    /// A collection named through `std::` or `alloc::`; argument: the path's first segment.
    AmbientCollection = 52,
    /// An `unsafe` block or impl with no `// SAFETY:` comment above it.
    UnsafeNoComment = 60,
    /// A formatting macro; argument: its name's range.
    FormatMacro = 70,
    /// An old name whose receiver is known to be blaze-own's, with its fix; argument: the
    /// row of [`RuleSet::renames`].
    RenamedFixed = 80,
    /// An old name only blaze-own has, on a receiver of unknown type: for a person; argument:
    /// the row.
    RenamedReport = 81,
    /// The process's heap reached for (`Alloc::global(`, `Heap::process(`); argument: the row of
    /// the sequence table, whose words say what to take instead.
    HeapReach = 90,
    /// A std constructor that allocates with no allocator named (`Vec::new(`); argument: the
    /// row. Whether the type is std's is not known from text (a user type may be named `Vec`).
    HeapConstructor = 91,
    /// An allocating macro (`vec!`, `format!`); argument: the row.
    HeapMacro = 92,
    /// An allocating method (`.to_string(`, `.to_owned(`, `.to_vec(`); argument: the row.
    HeapMethod = 93,
    /// `async` before `fn`, `unsafe`, `move`, `{` or `|`.
    Async = 100,
    /// `.await`.
    Await = 101,
    /// The clock read (`Instant::now(`); argument: the row.
    ReadClock = 110,
    /// The environment read (`env::var(`); argument: the row.
    ReadEnv = 111,
    /// Randomness read (`thread_rng(`); argument: the row.
    ReadRandom = 112,
    /// A hash seed drawn from the process (`HashMap::new(`); argument: the row.
    ReadHashSeed = 113,
    /// The process's own state (`process::exit(`, `process::id(`); argument: the row.
    ReadProcess = 114,
    UndoneTodo = 120,
    UndoneUnimplemented = 121,
    /// `dbg!`; fixed, by taking the macro away, where its argument is a value (a call, a
    /// literal), so the program keeps the value and loses only the print.
    UndoneDbg = 122,
    /// `unreachable!()` with no message.
    UndoneUnreachable = 123,
    /// A wait call with no argument (`.recv()`, `.join()`, `.wait()`, `.lock()`); argument: the
    /// row.
    WaitCall = 130,
    /// A `loop` whose braces hold no `break`, `return` or `?`.
    WaitLoop = 131,
    /// A std or hashbrown table or hasher named by its path (`collections::HashMap`,
    /// `hash_map::RandomState`, `hashbrown::HashSet`); argument: the row.
    HasherStd = 140,
    /// A constructor that builds its table's hasher from `Default` (`HashMap::new_in(`);
    /// argument: the row, whose words are the explicit form.
    HasherDefault = 141,
    /// A literal that is the variant count of one enum of the file; arguments: why it is not
    /// fixed (`variants::FIXED` when it is) and the enum's name. Fixed to
    /// `core::mem::variant_count::<E>()` where the bare name reaches it and the type is `usize`.
    VariantCount = 150,
    /// A literal that is the variant count of two or more enums of the file: which it counts is
    /// for a person; arguments: a placeholder, then each enum's name.
    VariantCountMany = 151,
}

impl Template {
    pub const fn rule(self) -> Rule {
        match self as u16 {
            1 => Rule::Casts,
            2..=5 => Rule::Clones,
            10..=13 => Rule::Allocs,
            20..=24 => Rule::Untyped,
            30 => Rule::Recursion,
            40..=46 => Rule::Stops,
            50..=52 => Rule::Ambient,
            60 => Rule::UnsafeComment,
            70 => Rule::FormatMacro,
            80..=81 => Rule::Renamed,
            90..=93 => Rule::AmbientHeap,
            100..=101 => Rule::NoAsync,
            110..=114 => Rule::AmbientRead,
            120..=123 => Rule::LeftUndone,
            130..=131 => Rule::UnboundedWait,
            140..=141 => Rule::AmbientHasher,
            _ => Rule::VariantCount,
        }
    }

    /// The kind's name, as the scripts name their kinds (`blaze-findings/1`'s `kind`).
    pub const fn kind(self) -> &'static str {
        match self {
            Template::CastToInt => "cast",
            Template::CloneHandle => "handle",
            Template::CloneErrorPath => "error-path",
            Template::CloneDeep => "deep",
            Template::CloneUnknown => "unknown",
            Template::AllocGlobal => "global",
            Template::AllocSignature => "signature",
            Template::AllocComment => "comment",
            Template::AllocExpect => "expect",
            Template::UntypedString => "string",
            Template::UntypedStr => "str",
            Template::UntypedBoxed => "boxed",
            Template::UntypedAnyhow => "anyhow",
            Template::UntypedAlias => "alias",
            Template::Recursion => "recursion",
            Template::StopUnwrap => "unwrap",
            Template::StopExpect => "expect",
            Template::StopPanic => "panic",
            Template::StopUnreachable => "unreachable",
            Template::StopTodo => "todo",
            Template::StopUnimplemented => "unimplemented",
            Template::StopIndex => "index",
            Template::AmbientStd => "std",
            Template::AmbientAlloc => "alloc",
            Template::AmbientCollection => "vec_box",
            Template::UnsafeNoComment => "no-safety-comment",
            Template::FormatMacro => "format",
            Template::RenamedFixed => "fixed",
            Template::RenamedReport => "for-a-person",
            Template::HeapReach => "reach",
            Template::HeapConstructor => "constructor",
            Template::HeapMacro => "macro",
            Template::HeapMethod => "method",
            Template::Async => "async",
            Template::Await => "await",
            Template::ReadClock => "clock",
            Template::ReadEnv => "env",
            Template::ReadRandom => "random",
            Template::ReadHashSeed => "hash-seed",
            Template::ReadProcess => "process",
            Template::UndoneTodo => "todo",
            Template::UndoneUnimplemented => "unimplemented",
            Template::UndoneDbg => "dbg",
            Template::UndoneUnreachable => "unreachable-no-message",
            Template::WaitCall => "call",
            Template::WaitLoop => "loop",
            Template::HasherStd => "std-table",
            Template::HasherDefault => "default-hasher",
            Template::VariantCount => "count",
            Template::VariantCountMany => "count-many",
        }
    }
}

/// One argument of a message: a range of the item's text, an integer (a table row), or text
/// from outside the item (a name the file declares elsewhere), a run of [`Findings::text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Arg {
    Span(Span),
    Int(u32),
    Text(Run),
}

/// One finding of one rule in one item (rule 81): no level, an item-relative range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Finding {
    pub template: Template,
    pub range: Span,
    /// Its message's arguments, in [`Findings::args`].
    pub args: Run,
    /// Its fixes, in [`Findings::fixes`]; empty when no mechanical fix is correct.
    pub fixes: Run,
}

impl Finding {
    #[inline]
    pub const fn rule(&self) -> Rule {
        self.template.rule()
    }
}

/// One way to fix a finding: its edits, in [`Findings::edits`], applied together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fix {
    pub edits: Run,
}

/// One edit: the item-relative `range` replaced by the bytes `text` names in [`Findings::text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Edit {
    pub range: Span,
    pub text: Run,
}

/// The findings of one or more items, as columns in the caller's allocator: what a finding
/// record holds.
pub struct Findings<'a> {
    pub list: AVec<'a, Finding>,
    pub args: AVec<'a, Arg>,
    pub fixes: AVec<'a, Fix>,
    pub edits: AVec<'a, Edit>,
    pub text: AVec<'a, u8>,
}

impl<'a> Findings<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Findings<'a> {
        Findings {
            list: AVec::new_in(alloc),
            args: AVec::new_in(alloc),
            fixes: AVec::new_in(alloc),
            edits: AVec::new_in(alloc),
            text: AVec::new_in(alloc),
        }
    }

    /// Empties every column, keeping their memory.
    pub fn clear(&mut self) {
        self.list.clear();
        self.args.clear();
        self.fixes.clear();
        self.edits.clear();
        self.text.clear();
    }

    pub fn args_of(&self, finding: &Finding) -> &[Arg] {
        &self.args[finding.args.range()]
    }

    pub fn fixes_of(&self, finding: &Finding) -> &[Fix] {
        &self.fixes[finding.fixes.range()]
    }

    pub fn edits_of(&self, fix: &Fix) -> &[Edit] {
        &self.edits[fix.edits.range()]
    }

    /// The replacement text of `edit`.
    pub fn text_of(&self, edit: &Edit) -> &str {
        self.text_in(edit.text)
    }

    /// The run `run` of the text column: an edit's replacement or an [`Arg::Text`].
    pub fn text_in(&self, run: Run) -> &str {
        // The text column holds only whole `&str`s pushed by the rules (`push_edit`,
        // variants.rs's names, copied from the file's text at token bounds).
        match self.text.get(run.range()).map(core::str::from_utf8) {
            Some(Ok(text)) => text,
            _ => unreachable_text(),
        }
    }

    fn push_edit(&mut self, range: Span, text: &str) -> Result<(), LintError> {
        let from = count(self.text.len())?;
        self.text.extend_from_slice(text.as_bytes());
        let to = count(self.text.len())?;
        self.edits.push(Edit { range, text: Run { from, to } });
        Ok(())
    }
}

#[cold]
fn unreachable_text() -> &'static str {
    ""
}

/// Which test code a site is in: `#[cfg(test)]` code (and test files), test functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct TestBits(pub u8);

impl TestBits {
    pub const NONE: TestBits = TestBits(0);
    /// `#[cfg(test)]`, `#[cfg(all(test, ..))]`, `#![cfg(test)]`, a test file.
    pub const CFG: TestBits = TestBits(1);
    /// `#[test]`, `#[x::test]`.
    pub const ATTR: TestBits = TestBits(2);
    pub const ANY: TestBits = TestBits(3);

    #[inline]
    pub const fn union(self, other: TestBits) -> TestBits {
        TestBits(self.0 | other.0)
    }

    #[inline]
    pub const fn meets(self, other: TestBits) -> bool {
        self.0 & other.0 != 0
    }
}

/// The kind of the innermost `{` block a site is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
#[repr(u8)]
pub enum Block {
    /// Directly in the file.
    #[default]
    File = 0,
    /// An inline `mod m { .. }`.
    Module = 1,
    /// `impl Type { .. }`.
    Impl = 2,
    /// `impl Trait for Type { .. }`: its methods' signatures are the trait's.
    TraitImpl = 3,
    /// `trait T { .. }`.
    Trait = 4,
    /// A function's body.
    Fn = 5,
    /// Any other block.
    Other = 6,
}

impl Block {
    /// An `impl` or `trait` block: a function directly in it is associated.
    #[inline]
    pub const fn container(self) -> bool {
        matches!(self, Block::Impl | Block::TraitImpl | Block::Trait)
    }
}

/// The comment marks a site's rule looks for above it: what the first line that decides is,
/// counting lines up from a reference line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Walk {
    /// How many lines above the reference line the deciding line is (1: the line just above);
    /// 0 when no line decides within the look-up.
    pub dist: u8,
    pub mark: Mark,
}

impl Walk {
    pub const NONE: Walk = Walk { dist: 0, mark: Mark::Stop };
}

/// What a deciding line says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
#[repr(u8)]
pub enum Mark {
    /// The line ends the look: no mark is this site's.
    #[default]
    Stop = 0,
    /// `// clone:` saying a refcount (handle words).
    Handle = 1,
    /// `// clone:` saying the error path.
    ErrorPath = 2,
    /// `// clone:` saying a deep copy.
    Deep = 3,
    /// `// clone:` saying none of the three.
    Unsorted = 4,
    /// A `// SAFETY:` comment.
    Safety = 5,
}

/// What the parent's text around a child item says that the child's rules read: the comments
/// above it and after it on its last line. Computed by the parent's pass ([`Linted::children`]),
/// part of the child's [`Scope::key`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Context {
    /// The `// clone:` walk from the line above the item's first line.
    pub clone_above: Walk,
    /// A `// clone:` mark after the item on its last line.
    pub clone_after: Option<Mark>,
    /// The `// SAFETY:` walk from the line above the item's first line.
    pub safety_above: Walk,
    /// A `// SAFETY:` mark after the item on its last line.
    pub safety_after: bool,
    /// How many lines above the item's first line the nearest `// recursion: calls another
    /// impl` mark is, within three; 0 for none.
    pub recursion_above: u8,
}

/// What a site's rules read from outside its item: the file's facts, and what the parent items'
/// text says of where the item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Scope {
    /// The test code the item is in (its file's, or an attribute of an ancestor's).
    pub test: TestBits,
    /// The innermost block the item is directly in.
    pub block: Block,
    /// The nearest enclosing function body or container is a container (`impl`, `trait`).
    pub assoc: bool,
    /// Some ancestor is an inline `mod`.
    pub in_module: bool,
    /// The file is a binary's crate root (`src/main.rs`, `src/bin/*.rs`, `examples/*.rs`).
    pub bin_root: bool,
    /// The file is a build script (`build.rs` outside `src`).
    pub build_script: bool,
    pub context: Context,
}

impl Scope {
    /// A file's root scope, from its facts.
    pub const fn file(test: TestBits, bin_root: bool, build_script: bool) -> Scope {
        Scope {
            test,
            block: Block::File,
            assoc: false,
            in_module: false,
            bin_root,
            build_script,
            context: Context {
                clone_above: Walk::NONE,
                clone_after: None,
                safety_above: Walk::NONE,
                safety_after: false,
                recursion_above: 0,
            },
        }
    }

    /// Everything in the scope as one integer, for the record's key: two scopes with one key
    /// are equal.
    pub const fn key(&self) -> u64 {
        let c = &self.context;
        let after = match c.clone_after {
            None => 0,
            Some(mark) => 1 + mark as u64,
        };
        (self.test.0 as u64)
            | (self.block as u64) << 2
            | (self.assoc as u64) << 5
            | (self.in_module as u64) << 6
            | (self.bin_root as u64) << 7
            | (self.build_script as u64) << 8
            | (c.clone_above.dist as u64) << 9
            | (c.clone_above.mark as u64) << 13
            | after << 16
            | (c.safety_above.dist as u64) << 19
            | (c.safety_above.mark as u64) << 23
            | (c.safety_after as u64) << 26
            | (c.recursion_above as u64) << 27
    }
}

/// The two kinds of input a rule can take. Only [`Input::Text`] is built: every rule of this
/// crate is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// The item's own text and tokens, and its [`Scope`].
    Text,
    /// The item's typed body record too, keyed by its own key (not built).
    Typed,
}

/// The receiver types an old name of [`RuleSet::renames`] is blaze-own's on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Owner {
    /// `blaze_own::Vec`, `AVec`.
    Vec = 0,
    /// `blaze_own::VecDeque`, `ADeque`.
    Deque = 1,
    /// `blaze_own::FixedVec`.
    Fixed = 2,
    /// `blaze_own::Soa`, `ASoa`.
    Soa = 3,
}

/// One row of the rename table: on `owner`, the method `old` is now `new`, with `suffix` put
/// after the call's `)` (`contains(x)` is `scan_for(x).is_some()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rename {
    pub owner: Owner,
    pub old: &'static str,
    pub new: &'static str,
    pub suffix: &'static str,
}

const fn row(owner: Owner, old: &'static str, new: &'static str, suffix: &'static str) -> Rename {
    Rename { owner, old, new, suffix }
}

/// The table seeded from blaze-own's `#[deprecated]` methods: each old name forwards to its new
/// one, so the rename keeps the program's meaning.
pub const RENAMES: [Rename; 15] = [
    row(Owner::Vec, "insert", "insert_shift", ""),
    row(Owner::Vec, "remove", "remove_shift", ""),
    row(Owner::Vec, "swap_remove", "remove_swap", ""),
    row(Owner::Vec, "insert_many", "insert_slice_shift", ""),
    row(Owner::Vec, "try_insert_many", "try_insert_slice_shift", ""),
    row(Owner::Vec, "contains", "scan_for", ".is_some()"),
    row(Owner::Vec, "position", "scan_for", ""),
    row(Owner::Fixed, "insert", "insert_shift", ""),
    row(Owner::Fixed, "remove", "remove_shift", ""),
    row(Owner::Fixed, "swap_remove", "remove_swap", ""),
    row(Owner::Deque, "insert", "insert_shift", ""),
    row(Owner::Deque, "remove", "remove_shift", ""),
    row(Owner::Soa, "swap_remove", "remove_swap", ""),
    row(Owner::Soa, "insert", "insert_shift", ""),
    row(Owner::Soa, "remove", "remove_shift", ""),
];

/// The old names no collection but blaze-own's `Vec` has: a call of one on a receiver of
/// unknown type is reported for a person (its `Vec` row), not fixed.
const ONLY_OWN: [&str; 2] = ["insert_many", "try_insert_many"];

/// Which rules run, with their tables: what [`RuleSet::id`] digests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleSet {
    enabled: u32,
    renames: &'static [Rename],
}

impl RuleSet {
    /// Every rule, with [`RENAMES`].
    pub const ALL: RuleSet = RuleSet { enabled: u32::MAX, renames: &RENAMES };

    /// No rule runs: a driver makes no lint task for it.
    pub const fn is_empty(&self) -> bool {
        self.enabled == 0
    }

    /// Only `rules`.
    pub const fn only(rules: &[Rule]) -> RuleSet {
        let mut enabled = 0;
        let mut at = 0;
        // Bounded: one turn per rule given.
        while at < rules.len() {
            enabled |= rules[at].bit();
            at += 1;
        }
        RuleSet { enabled, renames: &RENAMES }
    }

    /// `self` and `rule`: how a workspace adds a rule its step does not run.
    pub const fn with(self, rule: Rule) -> RuleSet {
        RuleSet { enabled: self.enabled | rule.bit(), renames: self.renames }
    }

    #[inline]
    pub const fn runs(&self, rule: Rule) -> bool {
        self.enabled & rule.bit() != 0
    }

    pub const fn renames(&self) -> &'static [Rename] {
        self.renames
    }

    /// The rule set's identity: a digest of the format, each enabled rule's id and version, and
    /// the rename table, in order. It joins every finding record's key.
    pub fn id(&self) -> [u8; 32] {
        let mut hasher = blaze_blake3::Hasher::new_derive_key("blaze-lint rule set 1");
        for rule in Rule::ALL {
            if self.runs(rule) {
                hasher.update(&rule.id().to_le_bytes());
                hasher.update(&rule.version().to_le_bytes());
            }
        }
        for rename in self.renames {
            hasher.update(&[rename.owner as u8]);
            for part in [rename.old, rename.new, rename.suffix] {
                hasher.update(&(part.len() as u32).to_le_bytes());
                hasher.update(part.as_bytes());
            }
        }
        hasher.finalize()
    }
}

/// What a finding is when printed: decided only then, from configuration, never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Printed before the answer, and the command fails.
    Error,
    /// Printed after the answer.
    Advice,
    Off,
}

/// A level per rule: the configuration's, read at print time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Levels(pub [Level; Rule::COUNT]);

impl Levels {
    /// Everything advice: the scripts' ratchets are counts that go down, not errors yet.
    pub const ADVICE: Levels = Levels([Level::Advice; Rule::COUNT]);

    pub const fn of(&self, rule: Rule) -> Level {
        self.0[rule as usize - 1]
    }
}

/// The step of AGENTS.md's adoption ladder a project is on: which of these rules it gets, and at
/// which level (docs/v2/plan/design/lint.md, "Which rules a project gets"). A project that names
/// none is on [`Step::Rust`]: every rule here is one of blaze's house rules, which a plain Rust
/// project did not choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// None of these rules: only what Rust itself refuses.
    Rust,
    /// Advice for what is a defect class in any Rust: an `unsafe` with no stated assumption, an
    /// untyped error, a function that calls itself.
    Suggest,
    /// And what the blaze edition makes an error later: stops, casts, clones; still advice.
    Edition,
    /// The ten house rules ([`Rule::HOUSE`]), at blaze's own levels, and `variant-count`, which
    /// every step from [`Step::Suggest`] runs. The other rules after them (`ambient-heap`,
    /// `no-async`, `ambient-read`, `left-undone`, `unbounded-wait`, `ambient-hasher`) are on only
    /// where a workspace names them: a step adds them with [`RuleSet::with`].
    Strict,
}

impl Step {
    /// The rules of [`Step::Suggest`]. `variant-count` is a defect class in any Rust (a count
    /// that a new variant leaves stale); its fix names a nightly feature, which blaze's own
    /// toolchain has.
    pub const SUGGEST: [Rule; 4] = [Rule::UnsafeComment, Rule::Untyped, Rule::Recursion, Rule::VariantCount];
    /// The rules of [`Step::Edition`].
    pub const EDITION: [Rule; 7] =
        [Rule::UnsafeComment, Rule::Untyped, Rule::Recursion, Rule::VariantCount, Rule::Stops, Rule::Casts, Rule::Clones];

    /// The step as configuration writes it (`rust`, `suggest`, `edition`, `strict`).
    pub fn from_name(name: &str) -> Option<Step> {
        match name {
            "rust" => Some(Step::Rust),
            "suggest" => Some(Step::Suggest),
            "edition" => Some(Step::Edition),
            "strict" => Some(Step::Strict),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Step::Rust => "rust",
            Step::Suggest => "suggest",
            Step::Edition => "edition",
            Step::Strict => "strict",
        }
    }

    /// The rules this step runs. [`Step::Rust`] runs none, so a driver makes no lint task.
    pub const fn rules(self) -> RuleSet {
        match self {
            Step::Rust => RuleSet::only(&[]),
            Step::Suggest => RuleSet::only(&Step::SUGGEST),
            Step::Edition => RuleSet::only(&Step::EDITION),
            Step::Strict => RuleSet::only(&Rule::HOUSE).with(Rule::VariantCount),
        }
    }

    /// The levels this step prints its findings at: advice for every token rule (one a step
    /// does not run makes no finding). A typed rule ([`Rule::typed`]) files its findings at
    /// every step, so its level is what turns it on: `args-inline` is off below
    /// [`Step::Edition`] (a blaze-edition API rule), advice there, an error in [`Step::Strict`].
    pub const fn levels(self) -> Levels {
        let mut levels = Levels::ADVICE.0;
        levels[Rule::ArgsInline as usize - 1] = match self {
            Step::Rust | Step::Suggest => Level::Off,
            Step::Edition => Level::Advice,
            Step::Strict => Level::Error,
        };
        Levels(levels)
    }
}

/// One item to lint: its whole text (its range of the file, its children's text included), its
/// children's ranges (item-relative, in order, apart), and its scope.
#[derive(Clone, Copy, Debug)]
pub struct Unit<'t> {
    pub text: &'t str,
    pub holes: &'t [Span],
    pub scope: Scope,
}

/// What a pass gives besides its findings.
pub struct Linted<'a> {
    /// The findings it pushed, in [`Findings::list`].
    pub findings: Run,
    /// Each hole's scope, in order: the scope its child item is linted in.
    pub children: AVec<'a, Scope>,
    /// The test code the item's last byte is in (an out-of-line `mod m;` declared in test
    /// code makes `m`'s file test code).
    pub test_at_end: TestBits,
}

/// Why a unit could not be linted: a bad input, named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LintError {
    /// The text is longer than a `u32` counts.
    TooLong { len: usize },
    /// A hole is out of order, overlaps the one before, ends past the text, or splits a
    /// character.
    BadHole { index: usize, hole: Span },
    /// A column would hold more entries than a `u32` counts.
    TooMany,
    /// A file's table would pass its cap (rule 18): what it counts, and the cap.
    Capped { what: &'static str, cap: u32 },
}

#[inline]
fn count(n: usize) -> Result<u32, LintError> {
    u32::try_from(n).map_err(|_| LintError::TooMany)
}

/// Lints one item that is its whole file (a test's text, a one-item file): [`lint_in`] with the
/// file's [`Variants`] gathered from `unit.text` itself, when `variant-count` runs. A driver
/// linting the units of a larger file calls [`lint_in`] with the file's fact, gathered once.
pub fn lint<'a>(rules: &RuleSet, unit: &Unit<'_>, alloc: Alloc<'a>, out: &mut Findings<'a>) -> Result<Linted<'a>, LintError> {
    let variants = match rules.runs(Rule::VariantCount) {
        true => Variants::gather(unit.text, alloc)?,
        false => Variants::new_in(alloc),
    };
    lint_in(rules, unit, &variants, 0, alloc, out)
}

/// Lints one item of a file: every rule of `rules` in one pass over the tokens of `unit`'s own
/// bytes, findings pushed onto `out`, scratch in `alloc` (freed on return); no recursion, no
/// allocation per token. `variant-count` reads `variants`, the fact of the file `unit` is the
/// range from `base` of ([`Variants::gather`]); its findings follow the pass's.
pub fn lint_in<'a>(
    rules: &RuleSet,
    unit: &Unit<'_>,
    variants: &Variants<'_>,
    base: u32,
    alloc: Alloc<'a>,
    out: &mut Findings<'a>,
) -> Result<Linted<'a>, LintError> {
    let mut linted = engine::run(rules, unit, alloc, out)?;
    if rules.runs(Rule::VariantCount) {
        variants.find(unit, base, out)?;
        linted.findings.to = count(out.list.len())?;
    }
    Ok(linted)
}

/// The tokens of a whole text, counted: what the floor of a lint pass over it is (one lex). The
/// example times it beside [`lint`].
pub fn lex_floor(text: &str) -> usize {
    blaze_lex::tokenize(text, blaze_lex::FrontmatterAllowed::No, blaze_lex::Cpu::BASELINE).count()
}
