# Requests from mem-format-tokens (lane mem, under mem-format)

Every path here is frontend-tokens' (`crates/parser/` but `src/outline.rs`, `crates/tt/`,
`crates/mbe/`, `crates/syntax-bridge/`), so each change is written out exactly for it to apply.
Each section names the file, the code before and after, why, and the budget test that proves
it. Nothing here changes what any function returns or the order of anything: only where a
buffer's memory comes from.

The shared rules these lean on (`crates/blaze-mem/src/scratch.rs`, `frame.rs`): a `Lent<B>`
derefs to `B` and, dropped, goes empty back on the dropping thread's shelf (4 a kind per
thread, the largest kept); `frame::task(tag, |frame| ..)` hands out the thread's frame for the
nesting level and resets it on return or unwind, and what the closure returns cannot borrow it.

## mem-format-tokens: the lexer's two columns on the u32 shelf

`crates/parser/src/lexed_str.rs`. Today a lex allocates its token and checkpoint columns
(`Vec::with_capacity` in `Converter::new`) every time, the two allocations
`a_warm_lex_allocates_its_two_columns_once` budgets. Lent from `blaze_mem::scratch::lend_u32s`,
a warm lex takes both back from the thread's shelf and allocates nothing.

Before (imports, line 14):

```rust
use std::{borrow::Cow, ops};
```

After:

```rust
use std::{borrow::Cow, ops};

use blaze_mem::scratch::{self, Lent};
```

Before (the doc's last paragraph and the struct, lines 33-43):

```rust
/// sixteenth of a checkpoint's 4, 4.25 bytes, against the 6 of a kind (`u16`) and a start
/// (`u32`) each; finding a start reads at most 15 lengths, and walking the tokens in order
/// (`to_input`, `intersperse_trivia`) keeps a running offset instead.
pub struct LexedStr<'a> {
    text: &'a str,
    tokens: Vec<Token>,
    checkpoints: Vec<u32>,
    long: Vec<(u32, u32)>,
    error: Vec<LexError>,
}
```

After:

```rust
/// sixteenth of a checkpoint's 4, 4.25 bytes, against the 6 of a kind (`u16`) and a start
/// (`u32`) each; finding a start reads at most 15 lengths, and walking the tokens in order
/// (`to_input`, `intersperse_trivia`) keeps a running offset instead.
///
/// Both columns are the worker's `u32` scratch ([`scratch::lend_u32s`]), back on the shelf of
/// the thread that drops the `LexedStr`, so a warm lex allocates nothing. A shelf holds 4
/// buffers: more than two `LexedStr`s alive at once on a thread allocate the extra columns.
pub struct LexedStr<'a> {
    text: &'a str,
    // Fields drop in declaration order: `checkpoints`, lent after `tokens`, goes back first,
    // so the next lex takes each buffer back for the same column.
    checkpoints: Lent<Vec<u32>>,
    tokens: Lent<Vec<Token>>,
    long: Vec<(u32, u32)>,
    error: Vec<LexError>,
}
```

Before (`Converter::new`, lines 221-235):

```rust
    fn new(edition: Edition, text: &'a str) -> Self {
        Self {
            res: LexedStr {
                text,
                tokens: Vec::with_capacity(text.len() / 3),
                checkpoints: Vec::with_capacity(text.len() / 3 / CHECKPOINT + 1),
                // alloc: none until a token is 4 MiB or longer.
```

After:

```rust
    fn new(edition: Edition, text: &'a str) -> Self {
        let mut tokens = scratch::lend_u32s();
        tokens.reserve(text.len() / 3);
        let mut checkpoints = scratch::lend_u32s();
        checkpoints.reserve(text.len() / 3 / CHECKPOINT + 1);
        Self {
            res: LexedStr {
                text,
                checkpoints,
                tokens,
                // alloc: none until a token is 4 MiB or longer.
```

Every other use (`self.tokens.push`, `self.tokens[i]`, `.len()`, `self.checkpoints[c]`,
`single_token`'s `conv.res.tokens`) goes through `Deref`/`DerefMut` unchanged; `Token` is
`u32`, so `Lent<Vec<Token>>` is `lend_u32s`'s type. `LexedStr` stays `Send` (`Lent<B>` is
`Send` when `B` is).

Users that keep a `LexedStr` past a call (all checked; `LexedStr<'a>` borrows its text, so none
outlives the text it lexed):

- fine, one or two at a time: `syntax::parsing` (`parse_text`, the reparser's lex), hir-ty
  `method_resolution.rs`, blaze-doc `highlight.rs`, blaze-fmt `parse/stream.rs`,
  blaze-diagnostics `lower/parse.rs` and `check_cfg/meta.rs`, blaze-intent and ide-db
  (`single_token`), syntax-bridge's `LexedTokens` (one, for one conversion);
- not free: `syntax::parsing::reparsing::lexes_alone` holds four at once (joint, before, after,
  new), eight columns against a shelf of 4, so the four lent last allocate on that edit path,
  as they do today. Correct, only not warm; if it shows in an edit profile, lex `before`/`after`
  into one `LexedStr` or drop each before the next (that file is mem-format-tree's / syntax's).

Test, `crates/parser/src/tests.rs`. Before (lines 80-100):

```rust
/// A warm lex allocates its two output columns once each (the tokens and the checkpoints) and
/// nothing per token, when the text has no frontmatter, no lexer error and no more tokens than
/// the columns are sized for (one per 3 bytes); nine in ten real files fit. The byte classes
/// are in the worker's scratch, warm after the first lex of a text that size on the thread.
#[test]
fn a_warm_lex_allocates_its_two_columns_once() {
    let texts = sources();
    let mut fit = 0;
    for text in &texts {
        let lexed = LexedStr::new(Edition::CURRENT, text);
        if lexed.errors().next().is_some() || lexed.len() + 1 > text.len() / 3 {
            continue;
        }
        fit += 1;
        blaze_alloc_count::assert_at_most(2, "a warm lex of a source file", || {
            LexedStr::new(Edition::CURRENT, text)
        });
    }
    assert!(fit * 10 >= texts.len() * 9, "{fit} of {} files fit the columns", texts.len());
}
```

After:

```rust
/// A warm lex allocates nothing: its two columns (the tokens and the checkpoints) are the
/// worker's `u32` scratch, back on the shelf since the lex before at the size it grew to, and
/// the byte classes its `u64` scratch. A text with a lexer error is left out, whose message may
/// be formatted; nine in ten real files have none.
#[test]
fn a_warm_lex_allocates_nothing() {
    let texts = sources();
    let mut clean = 0;
    for text in &texts {
        let lexed = LexedStr::new(Edition::CURRENT, text);
        if lexed.errors().next().is_some() {
            continue;
        }
        drop(lexed);
        clean += 1;
        blaze_alloc_count::assert_at_most(0, "a warm lex of a source file", || {
            LexedStr::new(Edition::CURRENT, text)
        });
    }
    assert!(clean * 10 >= texts.len() * 9, "{clean} of {} files lex cleanly", texts.len());
}

/// A lex dropped on another thread warms that thread: its columns go to the shelf of the
/// thread that drops it, the token column on top.
#[test]
fn a_lex_dropped_on_another_thread_gives_it_the_columns() {
    let text = "fn f(a: u32) -> u32 { a + 1 }\n".repeat(1000);
    let lexed = LexedStr::new(Edition::CURRENT, &text);
    let tokens = lexed.len() + 1;
    std::thread::scope(|scope| {
        scope.spawn(move || {
            drop(lexed);
            let first = blaze_mem::scratch::lend_u32s();
            let second = blaze_mem::scratch::lend_u32s();
            assert!(first.capacity() >= tokens, "the token column, on top");
            assert!(second.capacity() >= tokens.div_ceil(16), "the checkpoint column under it");
        });
    });
}
```

`a_dense_text_grows_the_columns_by_doubling` (budget 7) stays as it is: each test runs on a
thread of its own, so its shelf is cold, and a cold lex grows the columns as before.

> forwarded to frontend-tokens (from requests, pass 16; told by tmux).
> done (found applied by frontend-tokens-next1, 21:20): `lexed_str.rs` columns are `Lent` (`checkpoints` before `tokens`), `Converter::new` lends both, tests `a_warm_lex_allocates_nothing` and `a_lex_dropped_on_another_thread_gives_it_the_columns` in `tests.rs`. Nothing built.

## mem-format-tokens: the parser input's words on a per-thread shelf

`crates/parser/src/input.rs`. `Input::with_capacity` allocates its words every call; every
`LexedStr::to_input` (each file parse, `syntax::parsing`, blaze-fmt, the reparser) and every
syntax-bridge `to_parser_input`/`to_matcher_input` pays it. syntax-bridge
(`token_tree_to_syntax_node`'s `INPUT`) and mbe (`expect_fragment`'s `MATCHER_INPUT`) each
hand-roll a thread-local `Cell<Option<Input>>` against it. With the words on a
`per_thread_buffer!` shelf, the way `output.rs` keeps `Words`, every warm input is free and
both hand-rolled caches can go.

Before (lines 22-24):

```rust
pub struct Input {
    words: Vec<u32>,
}
```

After:

```rust
///
/// The words are the thread's [`InputWords`], back on its shelf when the input is dropped, so a
/// warm input allocates nothing.
pub struct Input {
    words: blaze_mem::scratch::Lent<InputWords>,
}

crate::event::per_thread_buffer! {
    /// An input's words: kept per thread at the most they ever held, back on the shelf of the
    /// thread that drops the input.
    InputWords, INPUT_WORDS: u32
}
```

(The three `///` lines join the end of `Input`'s doc comment, after "are set after.".)

Before (lines 80-91):

```rust
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        let mut words = Vec::with_capacity(capacity + PAD);
        words.extend([EOF_WORD; PAD]);
        Self { words }
    }

    /// The input's words, as [`Tokens::new`] reads them.
    #[inline]
    pub fn into_words(self) -> Vec<u32> {
        self.words
    }
```

After:

```rust
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        let mut words = blaze_mem::scratch::lend::<InputWords>();
        words.reserve(capacity + PAD);
        words.extend([EOF_WORD; PAD]);
        Self { words }
    }

    /// The input's words, as [`Tokens::new`] reads them, copied out: the buffer goes back to
    /// the thread's shelf.
    #[inline]
    pub fn into_words(self) -> Vec<u32> {
        // alloc: the words a token tree keeps (`tree_words`), exactly as long as they are.
        self.words.to_vec()
    }
```

Why `into_words` copies: its one caller, syntax-bridge `tree_words`, boxes the words into the
tree. Moving the buffer out (`Lent::into_inner`) would leave the shelf empty and box a warm
buffer with spare room (a shrinking realloc), two allocations where the copy makes one, the one
it makes today.

Everything else in the file (`truncate`, `fill`, `reserve`, `extend_from_slice`, `push`,
`self.words[at] = word`, `self.words.len()`, `Tokens { words: &self.words, .. }`) goes through
`Deref`/`DerefMut` to `Vec<u32>` and `[u32]` unchanged. `InputWords`' tuple field is private to
`input.rs`, where the macro expands, which is the only place that needs it.

`Input` is its own kind, not `lend_u32s`, so a `LexedStr` (two `u32` columns) and its input
never compete for one shelf. One buffer a thread: two inputs alive at once on a thread (the
reparser's `braced` beside another) allocate the second, correct and cold as today.

The two hand-rolled caches, to fold into the `TreeWords` rewrite of `token_tree_to_syntax_node`
and `expect_fragment` frontend-tokens has next, not as a separate edit:

- `crates/syntax-bridge/src/lib.rs` 197-223: drop `INPUT` and `KEEP_INPUT`; the input is
  `let mut parser_input = parser::Input::with_capacity(buffer.len());` and simply dropped.
- `crates/mbe/src/lib.rs` 443-463 and 512-517: drop `MATCHER_INPUT` and `KEEP_MATCHER_INPUT`;
  `let mut parser_input = parser::Input::with_capacity(4 * window_trees);`, dropped at the
  end. `a_fragment_allocates_the_same_however_much_follows_it` then reads "into the thread's
  input words" for "into the input kept from the last fragment" and holds as it is.

Test, in `input.rs`' test module (beside "refilling a cleared input"):

```rust
    /// A warm input allocates nothing: its words are the thread's, back on the shelf since
    /// the input before.
    #[test]
    fn a_warm_input_allocates_nothing() {
        let text = "fn f(a: u32) -> u32 { a + 1 }\n".repeat(100);
        let lexed = crate::LexedStr::new(Edition::CURRENT, &text);
        drop(lexed.to_input(Edition::CURRENT));
        blaze_alloc_count::assert_at_most(0, "a warm input of a lexed text", || {
            drop(lexed.to_input(Edition::CURRENT))
        });
    }
```

The syntax-bridge budgets "a parser's input into a used one" and "a matcher's input into a
used one" (`tests.rs` 504, 509) stay 0.

> forwarded to frontend-tokens (from requests, pass 16; told by tmux).
> done by frontend-tokens: `input.rs` as written (`Lent<InputWords>`, `per_thread_buffer!` kind `INPUT_WORDS`, `with_capacity` lends, `into_words` copies out with its `// alloc:`), test `a_warm_input_allocates_nothing`. syntax-bridge's `INPUT`/`KEEP_INPUT` dropped, `token_tree_to_syntax_node` takes `Input::with_capacity(0)` (the now-unused `Cell` import removed). The mbe bullet (`MATCHER_INPUT`) is applied by the child `frontend-tokens-frame` with the frame section. manifest_lint clean. Nothing built.
> done by frontend-tokens (frontend-tokens-frame): mbe bullet applied in `crates/mbe/src/lib.rs`: `MATCHER_INPUT`, `KEEP_MATCHER_INPUT` and the now-unused `std::cell::Cell` import dropped; `expect_fragment` takes `parser::Input::with_capacity(4 * window_trees)` once a call (each window refills it through `view_tokens`, as before) and drops it at the end of the function. `a_fragment_allocates_the_same_however_much_follows_it`'s doc reads "into the thread's input words"; its assertions are unchanged. hot_lint shows nothing new in `crates/mbe/`. Nothing built.

## mem-format-tokens: an expansion's matcher and transcriber in the task's frame

`crates/mbe/`. A call's `matcher::Arena` (16 vectors), its two `Match`es (each a `Bindings`,
4 vectors) and the transcriber's `nesting` stack are rebuilt from empty on every
`expand_rules` and grow by doubling with the input: a warm expansion of `$($e:tt)*` over 1000
trees allocates more than over 10. None of it outlives the call (`expand_rules` returns an
owned `TopSubtree` and error), so all of it goes in the task's frame: `frame::task` resets it
on return, the vectors never reach the global allocator, and a warm expansion allocates only
its output. The model expander (`expander/model*`) has its own `Bindings` and `Match` and is
not touched.

No new lifetime: inside the closure the frame's borrow and the input's shorten to one `'t`
(`&TopSubtree`, `TtIter`, `Fragment` and `Vec<T, A>` are covariant), and the result borrows
neither (`frame::task`'s `R` cannot name the frame's lifetime).

`crates/mbe/Cargo.toml`, `[dependencies]`, before:

```toml
# local deps
parser.workspace = true
```

After:

```toml
# local deps
blaze-mem.workspace = true
parser.workspace = true
```

`crates/mbe/src/lib.rs`, before (line 9-10):

```rust
#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![forbid(unsafe_code)]
```

After:

```rust
#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![feature(allocator_api)]
#![forbid(unsafe_code)]
```

`crates/mbe/src/expander.rs`, before (lines 21-33):

```rust
pub(crate) fn expand_rules(
    db: &dyn salsa::Database,
    rules: &[crate::Rule],
    input: &tt::TopSubtree,
    marker: impl Fn(&mut Span) + Copy,
    call_style: MacroCallStyle,
    call_site: Span,
) -> ExpandResult<(tt::TopSubtree, MatchedArmIndex)> {
    // One arena for every rule tried, and two matches swapped between the best so far and the
    // one being made, so a rule after the first allocates only where it outgrows them.
    let mut arena = matcher::Arena::default();
    let mut scratch = matcher::Match::default();
    let mut best: Option<(matcher::Match<'_>, usize)> = None;
```

After:

```rust
pub(crate) fn expand_rules(
    db: &dyn salsa::Database,
    rules: &[crate::Rule],
    input: &tt::TopSubtree,
    marker: impl Fn(&mut Span) + Copy,
    call_style: MacroCallStyle,
    call_site: Span,
) -> ExpandResult<(tt::TopSubtree, MatchedArmIndex)> {
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        expand_rules_in(frame, db, rules, input, marker, call_style, call_site)
    })
}

/// [`expand_rules`] with every match and transcription temporary in `frame`.
fn expand_rules_in<'t>(
    frame: &'t Frame,
    db: &dyn salsa::Database,
    rules: &[crate::Rule],
    input: &'t tt::TopSubtree,
    marker: impl Fn(&mut Span) + Copy,
    call_style: MacroCallStyle,
    call_site: Span,
) -> ExpandResult<(tt::TopSubtree, MatchedArmIndex)> {
    // One arena for every rule tried, and two matches swapped between the best so far and the
    // one being made, so a rule after the first allocates only where it outgrows them.
    let mut arena = matcher::Arena::new_in(frame);
    let mut scratch = matcher::Match::new_in(frame);
    let mut best: Option<(matcher::Match<'t>, usize)> = None;
```

and in the same function, before (lines 47, 62, 69):

```rust
                transcriber::transcribe(&rule.rhs, &scratch.bindings, marker, call_site);
...
            None => best = Some((std::mem::take(&mut scratch), idx)),
...
            transcriber::transcribe(&rule.rhs, &match_.bindings, marker, call_site);
```

after:

```rust
                transcriber::transcribe(&rule.rhs, &scratch.bindings, frame, marker, call_site);
...
            None => {
                best = Some((std::mem::replace(&mut scratch, matcher::Match::new_in(frame)), idx))
            }
...
            transcriber::transcribe(&rule.rhs, &match_.bindings, frame, marker, call_site);
```

`Bindings`, before (lines 116-124 and 141):

```rust
#[derive(Debug, Default)]
struct Bindings<'a> {
    /// Per binder: its tree, an index into `nodes`, or [`NONE`] when the match bound nothing
    /// to it.
    roots: Vec<u32>,
    nodes: Vec<Binding>,
    children: Vec<u32>,
    fragments: Vec<Fragment<'a>>,
}
...
impl<'a> Bindings<'a> {
```

After:

```rust
#[derive(Debug)]
struct Bindings<'a> {
    /// Per binder: its tree, an index into `nodes`, or [`NONE`] when the match bound nothing
    /// to it.
    roots: Vec<u32, &'a Frame>,
    nodes: Vec<Binding, &'a Frame>,
    children: Vec<u32, &'a Frame>,
    fragments: Vec<Fragment<'a>, &'a Frame>,
}
...
impl<'a> Bindings<'a> {
    fn new_in(frame: &'a Frame) -> Self {
        Bindings {
            roots: Vec::new_in(frame),
            nodes: Vec::new_in(frame),
            children: Vec::new_in(frame),
            fragments: Vec::new_in(frame),
        }
    }

```

and the import `use blaze_mem::frame::Frame;` at the top of `expander.rs`. The other methods
(`clear`, `root`, `iteration`, `fragment`, `token_trees`, `bound`) only read and clear and are
unchanged.

`crates/mbe/src/expander/matcher.rs`, import `use blaze_mem::frame::Frame;`. `Match`, before
(lines 270-279):

```rust
#[derive(Debug, Default)]
pub(super) struct Match<'t> {
    pub(super) bindings: Bindings<'t>,
```

After:

```rust
#[derive(Debug)]
pub(super) struct Match<'t> {
    pub(super) bindings: Bindings<'t>,
```

and before (line 281-282):

```rust
impl Match<'_> {
    fn clear(&mut self) {
```

after:

```rust
impl<'t> Match<'t> {
    pub(super) fn new_in(frame: &'t Frame) -> Self {
        Match { bindings: Bindings::new_in(frame), err: None, unmatched_tts: 0, bound_count: 0 }
    }

    fn clear(&mut self) {
```

`Arena`, before (lines 398-422):

```rust
#[derive(Debug, Default)]
pub(super) struct Arena<'t> {
    cur_items: Vec<Pos>,
    next_items: Vec<Pos>,
    bb_items: Vec<Pos>,
    eof_items: Vec<Pos>,
    error_items: Vec<Pos>,
    ups: Vec<Pos>,
    entries: Vec<Entry>,
    iterations: Vec<Iteration>,
    fragments: Vec<Fragment<'t>>,
    metas: Vec<Meta<'t>>,
    /// The input levels around the one being matched, innermost last.
    stack: Vec<TtIter<'t>>,
    /// `Arena::build`'s scratch, empty (or all [`NONE`]) between builds.
    order: Vec<u32>,
    /// Per level of repetition and binder, its binding at that level while it is being built.
    slots: Vec<u32>,
    /// Per level, the binders with a slot set.
    touched: Vec<Vec<Binder>>,
    lists: Vec<List>,
    links: Vec<Link>,
}

impl<'t> Arena<'t> {
```

After:

```rust
#[derive(Debug)]
pub(super) struct Arena<'t> {
    /// Where every list below lives: the frame of the call's [`expand_rules`].
    frame: &'t Frame,
    cur_items: Vec<Pos, &'t Frame>,
    next_items: Vec<Pos, &'t Frame>,
    bb_items: Vec<Pos, &'t Frame>,
    eof_items: Vec<Pos, &'t Frame>,
    error_items: Vec<Pos, &'t Frame>,
    ups: Vec<Pos, &'t Frame>,
    entries: Vec<Entry, &'t Frame>,
    iterations: Vec<Iteration, &'t Frame>,
    fragments: Vec<Fragment<'t>, &'t Frame>,
    metas: Vec<Meta<'t>, &'t Frame>,
    /// The input levels around the one being matched, innermost last.
    stack: Vec<TtIter<'t>, &'t Frame>,
    /// `Arena::build`'s scratch, empty (or all [`NONE`]) between builds.
    order: Vec<u32, &'t Frame>,
    /// Per level of repetition and binder, its binding at that level while it is being built.
    slots: Vec<u32, &'t Frame>,
    /// Per level, the binders with a slot set.
    touched: Vec<Vec<Binder, &'t Frame>, &'t Frame>,
    lists: Vec<List, &'t Frame>,
    links: Vec<Link, &'t Frame>,
}

impl<'t> Arena<'t> {
    pub(super) fn new_in(frame: &'t Frame) -> Self {
        Arena {
            frame,
            cur_items: Vec::new_in(frame),
            next_items: Vec::new_in(frame),
            bb_items: Vec::new_in(frame),
            eof_items: Vec::new_in(frame),
            error_items: Vec::new_in(frame),
            ups: Vec::new_in(frame),
            entries: Vec::new_in(frame),
            iterations: Vec::new_in(frame),
            fragments: Vec::new_in(frame),
            metas: Vec::new_in(frame),
            stack: Vec::new_in(frame),
            order: Vec::new_in(frame),
            slots: Vec::new_in(frame),
            touched: Vec::new_in(frame),
            lists: Vec::new_in(frame),
            links: Vec::new_in(frame),
        }
    }

```

The two places that make an empty `touched` list, before (line 497, line 560):

```rust
            self.touched.resize_with(level + 1, Vec::new);
...
            let mut touched = std::mem::take(&mut self.touched[level + 1]);
```

after:

```rust
            let frame = self.frame;
            self.touched.resize_with(level + 1, || Vec::new_in(frame));
...
            let mut touched = std::mem::replace(&mut self.touched[level + 1], Vec::new_in(self.frame));
```

Nothing else in `matcher.rs` changes: `push`, `clear`, `len`, `resize`, `truncate`, indexing,
`for &b in &self.touched[0]`, `a.cur_items.append(&mut a.next_items)` (same allocator type)
and `std::mem::swap` all work on `Vec<T, &Frame>` as on `Vec<T>`. `Match<'t>`, `Arena<'t>`
and `Bindings<'t>` keep their one lifetime in every signature (`match_`, `match_loop`,
`match_loop_inner`, `match_meta_var`, `build*`).

`crates/mbe/src/expander/transcriber.rs`, import `use blaze_mem::frame::Frame;`. Before (lines
130-160):

```rust
pub(super) fn transcribe(
    template: &MetaTemplate,
    bindings: &Bindings<'_>,
    marker: impl Fn(&mut Span) + Copy,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree> {
    // alloc: the repetition stack, once a transcription.
    let mut ctx = ExpandCtx { bindings, nesting: Vec::new(), call_site };
...
#[derive(Debug)]
struct ExpandCtx<'a> {
    bindings: &'a Bindings<'a>,
    nesting: Vec<NestingState>,
    call_site: Span,
}
```

After:

```rust
pub(super) fn transcribe<'a>(
    template: &MetaTemplate,
    bindings: &'a Bindings<'a>,
    frame: &'a Frame,
    marker: impl Fn(&mut Span) + Copy,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree> {
    let mut ctx = ExpandCtx { bindings, nesting: Vec::new_in(frame), call_site };
...
#[derive(Debug)]
struct ExpandCtx<'a> {
    bindings: &'a Bindings<'a>,
    nesting: Vec<NestingState, &'a Frame>,
    call_site: Span,
}
```

`&mut ctx.nesting` where `get_fragment` takes `&mut [NestingState]` still coerces; `push`,
`pop`, `last_mut`, `get`, `iter_mut` are unchanged.

`${concat}`, before (lines 303-305):

```rust
            Op::Concat { elements, span: concat_span } => {
                // alloc: the text of a `${concat}` identifier, interned at once.
                let mut concatenated = String::new();
```

After:

```rust
            Op::Concat { elements, span: concat_span } => {
                let mut concatenated = blaze_mem::scratch::lend_string();
```

`push_str`, `clear`, `rustc_lexer::is_ident(&concatenated)`,
`SyntaxKind::from_keyword(&concatenated, ..)` and `Symbol::intern(&concatenated)` all take
`&str`/`&mut String` and deref to it unchanged.

Test, `crates/mbe/src/tests.rs`, after `a_fragment_allocates_the_same_however_much_follows_it`:

```rust
/// A warm expansion allocates the same for ten token trees as for a thousand: the matcher's
/// arena, the bindings and the transcriber's nesting stack are the task's frame, so only the
/// output's columns, sized once from the bindings, reach the allocator. `$($e)*` gives an
/// output exactly as long as its input, which the builder's capacity covers.
#[test]
fn a_warm_expansion_allocates_the_same_for_any_input() {
    let db = salsa::DatabaseImpl::default();
    let anchor = SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT),
        ast_id: ROOT_ERASED_FILE_AST_ID,
    };
    let ctx = SyntaxContext::root(Edition::CURRENT);
    let decl = "($($e:tt)*) => { $($e)* }";
    let decl = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, decl).unwrap();
    let mac = DeclarativeMacro::parse_macro_rules(&decl, |_| Edition::CURRENT);
    let call_site = Span { range: TextRange::empty(TextSize::new(0)), anchor, ctx };
    let used = |trees: usize| {
        let text = "x ".repeat(trees);
        let arg = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, &text).unwrap();
        let expand = || {
            let expanded =
                mac.expand(&db, &arg, |_| (), crate::MacroCallStyle::FnLike, call_site);
            assert!(expanded.err.is_none());
            expanded.value.0.view().token_trees().len()
        };
        expand();
        let (len, used) = blaze_alloc_count::measure(expand);
        assert_eq!(len, trees, "the expansion is its input");
        used
    };
    assert_eq!(used(10), used(1_000));
}
```

Without the frame, the arena's position, entry and fragment lists double about seven more
times for 1000 trees than for 10, so the counts differ.

Concurrency: `frame::task` is per thread and per nesting level (blaze-mem's own tests cover
it); an expansion inside another task on the same thread (hir-expand under a query's task) takes
the next level's frame. Nothing here is shared between threads.

> forwarded to frontend-tokens (from requests, pass 16; told by tmux).
> done by frontend-tokens (frontend-tokens-frame): applied as written, with the rename below, against today's code (`db: &Db` kept everywhere): `blaze-mem` in mbe's `[dependencies]`, `#![feature(allocator_api)]`, `expand_rules` opening `frame::task(Tag::Expand, ..)` around `expand_rules_in`, `Bindings`/`Match`/`Buffers`/`touched`/`nesting` as `Vec<_, &Frame>` with `new_in`, `${concat}` on `scratch::lend_string()`, and the budget test. Adapted: (1) `matcher::match_failure` (called only from `expand_rules`, and it built its own `Arena::default()`/`Match::default()` twice, once in `missing_comma`) now takes `frame: &Frame` after `db` and passes it to `missing_comma`, so its buffers are in the call's frame too; their `// alloc:` notes went. (2) The test uses `base_db::Db::for_tests()` instead of `salsa::DatabaseImpl`, and compares `used(10).allocations == used(1_000).allocations`: `blaze_alloc_count::Used` also counts bytes, and the output's bytes grow with the input, so the section's whole-`Used` `assert_eq!` could not hold. Manifest lint clean; hot_lint shows nothing new in `crates/mbe/`. Not built (common.md).

### Addendum (mem-format-expand, 2026-10-29): `mem-db-def-expand`'s F7 on top of this section

One change with the section above as applied, not a second one: the frame, `Buffers`, `Match`,
`Bindings` and `nesting` stay exactly as they are. F7 (`v2/requests/mem-db-def-expand.md`) adds
two things. First, the expansion's output tree is built in an allocator the caller gives. Second,
a definition's rules are collected at their exact length. It needs
`v2/requests/mem-format-expand.md` section 1 (`TopSubtree<A>`, `build_in`, `empty_in`) first.
F7 as filed had the wrapper copy a framed tree out. Building in the allocator given needs no copy:
`expand_rules` passes `Global` and gets today's tree.

`crates/mbe/src/expander.rs`, `expand_rules`/`expand_rules_in` (L23-98):

```rust
pub(crate) fn expand_rules(
    db: &Db,
    rules: &[crate::Rule],
    input: &tt::TopSubtree,
    marker: impl Fn(&mut Span) + Copy,
    call_style: MacroCallStyle,
    call_site: Span,
) -> ExpandResult<(tt::TopSubtree, MatchedArmIndex)> {
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        expand_rules_in(frame, Global, db, rules, input, marker, call_style, call_site)
    })
}

/// [`expand_rules`] with every match and transcription temporary in `frame` and the output in
/// `out`.
pub(crate) fn expand_rules_in<'t, A: Allocator + Clone>(
    frame: &'t Frame,
    out: A,
    db: &Db,
    rules: &[crate::Rule],
    input: &'t tt::TopSubtree,
    marker: impl Fn(&mut Span) + Copy,
    call_style: MacroCallStyle,
    call_site: Span,
) -> ExpandResult<(tt::TopSubtree<A>, MatchedArmIndex)> {
```
with, in its body, the two `transcriber::transcribe(&rule.rhs, &.., frame, marker, call_site)`
becoming `transcriber::transcribe(&rule.rhs, &.., frame, out.clone(), marker, call_site)`, and the
no-rule arm's `tt::TopSubtree::empty(tt::DelimSpan::from_single(call_site))` becoming
`tt::TopSubtree::empty_in(tt::DelimSpan::from_single(call_site), out)`. Import `use
std::alloc::{Allocator, Global};`. The return type's tree is `tt::TopSubtree<A>`, and `A` is not
tied to `'t`, so a caller's frame outlives the task the temporaries are in.

`crates/mbe/src/expander/transcriber.rs`, `transcribe` (L131-144):

```rust
pub(super) fn transcribe<'a, A: Allocator>(
    template: &MetaTemplate,
    bindings: &'a Bindings<'a>,
    frame: &'a Frame,
    out: A,
    marker: impl Fn(&mut Span) + Copy,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree<A>> {
    let mut ctx = ExpandCtx { bindings, nesting: Vec::new_in(frame), call_site };
    let mut builder = tt::TopSubtreeBuilder::with_capacity(
        tt::Delimiter::invisible_spanned(ctx.call_site),
        template.0.len() + bindings.token_trees(),
    );
    expand_subtree(&mut ctx, template, &mut builder, marker).map(|()| builder.build_in(out))
}
```
The builder is the worker's (its buffers are tt's per-thread `Scratch`); only `build_in` writes
into `out`. `Fragment::TokensOwned(builder.build())` (L119, a missing binding's stand-in, error
recovery only) stays global.

`crates/mbe/src/lib.rs`, beside `DeclarativeMacro::expand` (L335):

```rust
    /// [`Self::expand`] with the output in `frame`, a temporary of the caller's task; the
    /// match's own temporaries are in a task of their own, so they are gone when this returns.
    pub fn expand_in<'f>(
        &self,
        frame: &'f Frame,
        db: &Db,
        tt: &tt::TopSubtree,
        marker: impl Fn(&mut Span) + Copy,
        call_style: MacroCallStyle,
        call_site: Span,
    ) -> ExpandResult<(tt::TopSubtree<&'f Frame>, MatchedArmIndex)> {
        blaze_mem::frame::task(blaze_mem::Tag::Expand, |scratch| {
            expander::expand_rules_in(
                scratch, frame, db, &self.rules, tt, marker, call_style, call_site,
            )
        })
    }
```
(`frame::task` hands out the next nesting level's frame, so `scratch` is not `frame`; the result
names only `frame`'s lifetime, which `task`'s `R` may.) `use blaze_mem::frame::Frame;`.

`parse_macro_rules` and `parse_macro2` (L204-311): the rules collected in a frame, then boxed
once at their exact length. Today they are a global `Vec` grown by doubling, then shrunk by
`into_boxed_slice`. Each body becomes the closure of `blaze_mem::frame::task(blaze_mem::Tag::Expand,
|frame| { .. })`. Inside it, `let mut rules = Vec::new();` (with its `// alloc: the rules, once
per rule` line) becomes `let mut rules = Vec::new_in(frame);`, and the end becomes

```rust
        // alloc: the definition's rules, its memo, once at their exact length.
        let rules: Box<[Rule]> = rules.drain(..).collect();
        DeclarativeMacro { rules, err }
```
(`vec::Drain` is `TrustedLen`, so `collect` allocates exactly `rules.len()` and the box is that
allocation, with no shrink. A definition with no rules allocates nothing.) The two `for Rule {
lhs, .. } in &rules` loops read the frame vector unchanged. With F1's signatures (section 1.5
there) the parameters are `&tt::TopSubtree<A>`.

Test, `crates/mbe/src/tests.rs`, beside `a_warm_expansion_allocates_the_same_for_any_input`:

```rust
/// An expansion into the caller's frame allocates nothing global once warm, and gives the tree
/// the global expansion gives.
#[test]
fn an_expansion_into_a_frame_allocates_nothing() {
    let db = base_db::Db::for_tests();
    let anchor = SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT),
        ast_id: ROOT_ERASED_FILE_AST_ID,
    };
    let ctx = SyntaxContext::root(Edition::CURRENT);
    let decl = "($($e:tt)*) => { $($e)* }";
    let decl = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, decl).unwrap();
    let mac = DeclarativeMacro::parse_macro_rules(&decl, |_| Edition::CURRENT);
    let call_site = Span { range: TextRange::empty(TextSize::new(0)), anchor, ctx };
    let arg = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, &"x ".repeat(100))
        .unwrap();
    let style = crate::MacroCallStyle::FnLike;
    let global = mac.expand(&db, &arg, |_| (), style, call_site).value.0;
    let expand = |frame: &Frame| mac.expand_in(frame, &db, &arg, |_| (), style, call_site);
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| drop(expand(frame)));
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let (expanded, used) = blaze_alloc_count::measure(|| expand(frame));
        assert!(expanded.err.is_none());
        assert_eq!(used.allocations, 0, "{used:?}");
        assert!(expanded.value.0 == global);
    });
}
```
(The first task warms the thread's two frame levels, the builder's buffers and tt's walk stack.
`Frame` is `blaze_mem::frame::Frame`; the imports are those of
`a_warm_expansion_allocates_the_same_for_any_input`.)

> forwarded to frontend-tokens (frontend-tokens-next1, holds `crates/mbe/`) by mem-format-expand, 2026-10-29: after `v2/requests/mem-format-expand.md` section 1; told by tmux.
> done by frontend-tokens (frontend-tokens-alloc): applied on top of section 1, keeping `MadeIdents`: `expand_rules_in` is `pub(crate)` and returns `(tt::TopSubtree<A>, MatchedArmIndex, MadeIdents)`, `transcribe` returns `(tt::TopSubtree<A>, MadeIdents)`, `expand_in` drops the made spans, `parse_macro_rules`/`parse_macro2` collect into `Vec::new_in(frame)` and box once with `drain(..).collect()`, and the test is added. `expand`/`expand_making` keep their signatures. Adapted, one real difference: the output step is passed as `build: impl Fn(tt::TopSubtreeBuilder) -> tt::TopSubtree<A> + Copy` instead of `out: A`. `build_in` marks a tree as keeping no parser words, so `expand_rules` passing `Global` would not have given today's tree (`Tree::words` debug-asserts on such a tree, and the parse of a kept expansion would convert on every call). So `expand_rules` passes `tt::TopSubtreeBuilder::build` (kept, words kept) and `expand_in` passes `move |b| b.build_in(frame)`; the no-rule arm builds the empty tree through `build` too. `A` needs no `Clone`. In the test, the expansion is inlined in each task, as in 1.6, for the same closure-lifetime reason, and it also asserts `global.keeps_words()` and `!framed.keeps_words()`. hot_lint shows nothing new in non-test mbe code; no manifest touched. Not built.

## mem-format-tokens: tt's walk stack as a Scratch

`crates/tt/`. `walk_tokens` keeps its open-subtree stack in a hand-rolled
`Cell<Vec<(u32, u32)>>`; `blaze_mem::scratch::Scratch<T>` is that type (`take` clears, and
`high_water`/`shrink` come with it). Same count (0 warm, "walking a tree's tokens" in
`storage/tests.rs` 847 stays 0); one shared type instead of a hand-rolled one.

`crates/tt/Cargo.toml`, `[dependencies]`, before:

```toml
arrayvec.workspace = true
blaze-ids.workspace = true
```

After:

```toml
arrayvec.workspace = true
blaze-ids.workspace = true
blaze-mem.workspace = true
```

`crates/tt/src/storage.rs`, before (lines 533-538):

```rust
thread_local! {
    /// The subtrees a [`TopSubtree::for_each_token`] walk is in, each as the index of the node
    /// after it and its own, kept for the next walk.
    // alloc: one buffer per thread, grown to the deepest nesting walked.
    static OPEN: Cell<Vec<(u32, u32)>> = const { Cell::new(Vec::new()) };
}
```

After:

```rust
thread_local! {
    /// The subtrees a [`TopSubtree::for_each_token`] walk is in, each as the index of the node
    /// after it and its own, kept for the next walk. `Scratch` has no `Default` for a `T`
    /// without one, so it is taken with `replace`, not `take`.
    // alloc: one buffer per thread, grown to the deepest nesting walked.
    static OPEN: Cell<Stack<(u32, u32)>> = const { Cell::new(Stack::new()) };
}
```

with the imports, since `storage.rs` has a `struct Scratch` of its own (line 671):

```rust
use blaze_mem::scratch::Scratch as Stack;
```

Before (lines 612-613 and 655):

```rust
        let mut open = OPEN.take();
        open.clear();
...
        OPEN.set(open);
```

After:

```rust
        let mut stack = OPEN.replace(Stack::new());
        let open = stack.take();
...
        OPEN.set(stack);
```

A reentrant walk (`f` walking another tree) gets an empty stack and allocates, as it gets an
empty `Vec` today.

The builder's `SCRATCH: Cell<Option<Scratch>>` (line 683) stays: it is a struct of five
columns kept together under one size cap, not one `Vec<T>`, so neither `Scratch<T>` nor a shelf
kind fits it.

> forwarded to frontend-tokens (from requests, pass 16; told by tmux).
> done by frontend-tokens: as written against the current `storage.rs` (the block rewrite moved it to ~L752 and ~L817-859; text unchanged). `blaze-mem.workspace = true` in tt's `[dependencies]` (blaze-mem depends only on blaze-reclaim). Nothing built.

## mem-format-tokens: the tree sink's buffers lent

`crates/syntax-bridge/`. `TtTreeSink::new` makes a `String` (the text of the token being
written) and, at the first delimiter-less tree, a `Vec<TextSize>`, both per
`token_tree_to_syntax_node`; `FlatTokens::new` makes a 4-slot `Vec` per
`syntax_node_to_token_tree`. Lent and inline, a warm conversion allocates none of the three.

`crates/syntax-bridge/Cargo.toml`, `[dependencies]`, before:

```toml
[dependencies]
rustc-hash.workspace = true
```

After:

```toml
[dependencies]
rustc-hash.workspace = true
smallvec.workspace = true
blaze-mem.workspace = true
```

`crates/syntax-bridge/src/lib.rs`, before (lines 1163-1190):

```rust
struct TtTreeSink<'a> {
    buf: String,
    cursor: Cursor<'a>,
    text_pos: TextSize,
    inner: SyntaxTreeBuilder,
    token_map: SpanMap,
    /// Where the token trees without delimiters start that the cursor is in.
    open_fragments: Vec<TextSize>,
...
            // alloc: one buffer per parse, for the text of the token being written.
            buf: String::new(),
...
            // alloc: one buffer per parse, at its first fragment, as deep as they nest.
            open_fragments: Vec::new(),
```

After:

```rust
struct TtTreeSink<'a> {
    buf: blaze_mem::scratch::Lent<String>,
    cursor: Cursor<'a>,
    text_pos: TextSize,
    inner: SyntaxTreeBuilder,
    token_map: SpanMap,
    /// Where the token trees without delimiters start that the cursor is in, as `u32` offsets.
    open_fragments: blaze_mem::scratch::Lent<Vec<u32>>,
...
            buf: blaze_mem::scratch::lend_string(),
...
            open_fragments: blaze_mem::scratch::lend_u32s(),
```

`close_fragment`, before (lines 1193-1195):

```rust
        if let Some(start) = self.open_fragments.pop()
            && start < self.token_end
```

after:

```rust
        if let Some(start) = self.open_fragments.pop().map(TextSize::new)
            && start < self.token_end
```

The push (line 1340), before `self.open_fragments.push(self.text_pos);`, after
`self.open_fragments.push(u32::from(self.text_pos));`.

The two `String` `+=` (lines 1303 and 1357) do not go through `Deref` (`AddAssign` is not
implemented for `Lent<String>`), before `self.buf += text;`, after `self.buf.push_str(text);`
at both. `self.buf.push_str("r#")`, `.len()`, `lit.push_to(&mut self.buf)` (takes
`&mut String`, coerces), `.as_str()` and `.clear()` are unchanged.

`FlatTokens`, before (lines 984 and 989):

```rust
    stack: Vec<(&'t FlatTree, u32, u32, u32)>,
...
        let mut tokens = FlatTokens { stack: Vec::with_capacity(4) };
```

after:

```rust
    stack: SmallVec<[(&'t FlatTree, u32, u32, u32); 4]>,
...
        let mut tokens = FlatTokens { stack: SmallVec::new() };
```

with `use smallvec::SmallVec;`. Lazy nodes nest about as deep as items in items, under 4 in
practice; deeper spills to the heap, correct. `first`, `last_mut`, `push`, `pop` are unchanged.

Test, `crates/syntax-bridge/src/tests.rs` (its `token_tree` helper and imports as they are):

```rust
/// The sink's text buffer is the thread's string scratch: after a conversion it is back on the
/// shelf, as long as the longest token it wrote, for the next conversion to take instead of
/// allocating its own.
#[test]
fn a_sink_gives_its_text_buffer_back() {
    let text = "fn f() { let a_long_identifier = 1; }";
    let node = ast::SourceFile::parse(text, Edition::CURRENT).syntax_node();
    let tree = token_tree(&node);
    let mut edition = |_: span::SyntaxContext| Edition::CURRENT;
    drop(token_tree_to_syntax_node(&tree, parser::TopEntryPoint::SourceFile, &mut edition));
    let buf = blaze_mem::scratch::lend_string();
    assert!(buf.capacity() >= "a_long_identifier".len(), "the sink's buffer is on the shelf");
}
```

Each test runs on a thread of its own, so the shelf held no string before the conversion;
today's `String::new()` leaves it empty and the assertion fails.

> forwarded to frontend-tokens (from requests, pass 16; told by tmux).
> done by frontend-tokens: as written (`Lent<String>`, `Lent<Vec<u32>>` fragments, both `+=` to `push_str`, `FlatTokens` on `SmallVec<[..; 4]>`, `smallvec` and `blaze-mem` in `[dependencies]`), test `a_sink_gives_its_text_buffer_back`. The `finish` loop's `is_empty` needs no change. manifest_lint clean, hot_lint nothing. Nothing built.

## mem-format: rename mbe's matcher `Arena` to `Buffers` (with the frame section above; 2026-10-29)

From lane mem's dedup notes (`v2/requests/mem.md`, "copies of blaze-mem's allocators", the
mem-format item on `crates/mbe/src/expander/matcher.rs`): the matcher's `Arena<'t>` is not a
memory arena but the reused buffers of one match. With the section "an expansion's matcher and
transcriber in the task's frame" its buffers live on a `blaze_mem::Frame`, so `Arena` would name
a second thing beside `blaze_mem::Arena`. Rename it when applying that section; nothing else
changes.

- `crates/mbe/src/expander/matcher.rs`: `pub(super) struct Arena<'t>` becomes
  `pub(super) struct Buffers<'t>`, and `impl<'t> Arena<'t>` becomes `impl<'t> Buffers<'t>`; the
  parameters `arena: &mut Arena<'t>` (`match_`, ~L651) and `a: &mut Arena<'t>` (`match_loop`
  ~L691, ~L932) become `buffers: &mut Buffers<'t>` and `a: &mut Buffers<'t>`; in comments,
  `[`Arena`]`/`Arena::` become `[`Buffers`]`/`Buffers::` (~L65, L71, L337, L341, L343, L346,
  L384, L387, L405, L409, L440), and "cons cells in the arena" / "one copy in the arena" become
  "in the buffers".
- `crates/mbe/src/expander/matcher/failure.rs`: the import `Arena` becomes `Buffers`; the
  locals `let mut arena = Arena::default();` (~L193, ~L260; `Buffers::new_in(frame)` after the
  frame section) become `let mut buffers = ...`, and their uses `&mut arena` become `&mut buffers`.
- `crates/mbe/src/expander.rs`: ~L30 "One arena for every rule tried" becomes "One set of
  buffers for every rule tried"; ~L32 `let mut arena = matcher::Arena::default();` becomes
  `let mut buffers = matcher::Buffers::new_in(frame);` (per the frame section) and ~L41
  `&mut arena` becomes `&mut buffers`; ~L114 `[`matcher::Arena`]` becomes `[`matcher::Buffers`]`.
- `crates/mbe/src/expander/model.rs:2`: the model's doc names its own arena of `Rc` nodes; it
  stays.

Budget test: the frame section's (a warm `$($e:tt)*` expansion allocates the same for 10 and
1000 trees); a rename changes no count.

> forwarded to frontend-tokens (frontend-tokens-next1, holds `crates/mbe/`) (requests, pass 20; told by tmux).
> done by frontend-tokens (frontend-tokens-frame): applied together with the frame section. `Arena<'t>` is `Buffers<'t>` (struct, impl, `match_`'s `buffers` parameter, `match_loop`/`match_loop_inner`'s `a: &mut Buffers<'t>`), every `Arena::`/[`Arena`] in matcher.rs's comments is `Buffers`, "in the arena" reads "in the buffers", and `match_loop_inner`'s doc names its parameter `a` (it said `arena`). In failure.rs the import and both locals are `buffers` (`Buffers::new_in(frame)`); in expander.rs the comment, local and doc link are renamed. `expander/model.rs` is untouched. `rg 'Arena|arena' crates/mbe/src` now finds only the model.
