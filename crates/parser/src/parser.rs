//! See [`Parser`].

use std::{borrow::Cow, num::NonZeroU32};

use crate::{
    Edition,
    SyntaxKind::{self, EOF, ERROR, TOMBSTONE},
    T, TokenSet,
    event::{self, Event, Events},
    input::Tokens,
};

/// Build a forward-parent offset. The offset is always ≥ 1 because the
/// forward-parent event is created *after* the event it forwards to, so
/// `NonZeroU32` is always valid here. Panics only on a parser bug.
#[inline]
fn fwd_parent(offset: u32) -> NonZeroU32 {
    NonZeroU32::new(offset).expect("forward-parent offset must be non-zero")
}

/// `Parser` struct provides the low-level API for
/// navigating through the stream of tokens and
/// constructing the parse tree. The actual parsing
/// happens in the [`grammar`](super::grammar) module.
///
/// However, the result of this `Parser` is not a real
/// tree, but rather a flat stream of events of the form
/// "start expression, consume number literal,
/// finish expression". See `Event` docs for more.
pub(crate) struct Parser<'t> {
    /// The tokens, an [`crate::Input`]'s or a token tree's words where they lie.
    inp: Tokens<'t>,
    pos: usize,
    /// The thread's event list, lent until [`event::process`] is done with it.
    events: blaze_mem::scratch::Lent<Events>,
    /// Side table of error messages. `Event::Error { err }` carries an index
    /// into this vec, keeping `Event` itself a flat 8-byte enum.
    errors: Vec<Cow<'static, str>>,
    /// Nodes started and errors made since the last token was consumed. A loop that consumes
    /// nothing still starts a node or makes an error each turn (or asks to bump at `EOF`), so
    /// counting those, and not every kind test, catches it.
    steps: u32,
    /// Whether `fn` bodies are skipped as lazy nodes ([`crate::outline`]).
    outline: bool,
    /// Which tokens are a `{` some `}` closes ([`Parser::curly_closes`]), made when first asked.
    closed_curlies: Option<Vec<bool>>,
}

const PARSER_STEP_LIMIT: usize = if cfg!(debug_assertions) { 150_000 } else { 15_000_000 };

impl<'t> Parser<'t> {
    pub(super) fn new(inp: impl Into<Tokens<'t>>) -> Parser<'t> {
        let inp = inp.into();
        Parser {
            inp,
            pos: 0,
            events: event::lend_events(inp.len()),
            errors: Vec::new(), // alloc: empty until the first syntax error, an error path
            steps: 0,
            outline: false,
            closed_curlies: None,
        }
    }

    pub(super) fn new_outline(inp: impl Into<Tokens<'t>>) -> Parser<'t> {
        Parser { outline: true, ..Parser::new(inp) }
    }

    /// In an outline, the number of tokens of the `fn` body at the current `{` when it can be
    /// left unparsed ([`crate::outline::lazy_block_len`]).
    pub(crate) fn lazy_body(&self) -> Option<u32> {
        if !self.outline {
            return None;
        }
        crate::outline::lazy_block_len(self.inp, self.pos)
    }

    /// Skips the next `n_input_tokens` tokens as one lazy node of `kind`.
    pub(crate) fn bump_lazy(&mut self, kind: SyntaxKind, n_input_tokens: u32) {
        self.pos += n_input_tokens as usize;
        self.steps = 0;
        self.push_event(Event::Lazy { kind, n_input_tokens });
    }

    /// Runs `parse` with every body in it parsed, in an outline too.
    pub(crate) fn in_full(&mut self, parse: impl FnOnce(&mut Parser<'t>)) {
        let outline = std::mem::replace(&mut self.outline, false);
        parse(self);
        self.outline = outline;
    }

    pub(crate) fn finish(self) -> (blaze_mem::scratch::Lent<Events>, Vec<Cow<'static, str>>) {
        (self.events, self.errors)
    }

    /// Returns the kind of the current token.
    /// If parser has already reached the end of input,
    /// the special `EOF` kind is returned.
    pub(crate) fn current(&self) -> SyntaxKind {
        self.nth(0)
    }

    /// Lookahead operation: returns the kind of the next nth
    /// token.
    pub(crate) fn nth(&self, n: usize) -> SyntaxKind {
        assert!(n <= 3);
        self.inp.kind(self.pos + n)
    }

    /// The index of the current token, for [`Parser::curly_closes`].
    pub(crate) fn token_index(&self) -> usize {
        self.pos
    }

    /// Whether a `}` closes the `{` at token `open` before the input ends, each `}` closing the
    /// nearest `{` before it not yet closed, as rustc's token trees pair them.
    pub(crate) fn curly_closes(&mut self, open: usize) -> bool {
        let inp = self.inp;
        let closed = self.closed_curlies.get_or_insert_with(|| {
            let mut closed = vec![false; inp.len()]; // alloc: once a parse, an error path
            let mut unclosed = Vec::new(); // alloc: once a parse, an error path
            for at in 0..inp.len() {
                match inp.kind(at) {
                    T!['{'] => unclosed.push(at),
                    T!['}'] => {
                        if let Some(it) = unclosed.pop() {
                            closed[it] = true;
                        }
                    }
                    _ => (),
                }
            }
            closed
        });
        closed.get(open).copied().unwrap_or(false)
    }

    /// Counts one step that consumed no token, and panics when there have been too many: the
    /// grammar is in a loop that makes no progress.
    #[inline]
    fn step(&mut self) {
        assert!((self.steps as usize) < PARSER_STEP_LIMIT, "the parser seems stuck");
        self.steps += 1;
    }

    /// Whether the current token starts a fragment a macro transcribed, which goes on no
    /// expression before it.
    pub(crate) fn at_fragment_start(&self) -> bool {
        self.inp.is_fragment_start(self.pos)
    }

    /// Checks if the current token is `kind`.
    pub(crate) fn at(&self, kind: SyntaxKind) -> bool {
        self.nth_at(0, kind)
    }

    pub(crate) fn nth_at(&self, n: usize, kind: SyntaxKind) -> bool {
        match kind {
            T![-=] => self.at_composite2(n, T![-], T![=]),
            T![->] => self.at_composite2(n, T![-], T![>]),
            T![::] => self.at_composite2(n, T![:], T![:]),
            T![!=] => self.at_composite2(n, T![!], T![=]),
            T![..] => self.at_composite2(n, T![.], T![.]),
            T![*=] => self.at_composite2(n, T![*], T![=]),
            T![/=] => self.at_composite2(n, T![/], T![=]),
            T![&&] => self.at_composite2(n, T![&], T![&]),
            T![&=] => self.at_composite2(n, T![&], T![=]),
            T![%=] => self.at_composite2(n, T![%], T![=]),
            T![^=] => self.at_composite2(n, T![^], T![=]),
            T![+=] => self.at_composite2(n, T![+], T![=]),
            T![<<] => self.at_composite2(n, T![<], T![<]),
            T![<=] => self.at_composite2(n, T![<], T![=]),
            T![==] => self.at_composite2(n, T![=], T![=]),
            T![=>] => self.at_composite2(n, T![=], T![>]),
            T![>=] => self.at_composite2(n, T![>], T![=]),
            T![>>] => self.at_composite2(n, T![>], T![>]),
            T![|=] => self.at_composite2(n, T![|], T![=]),
            T![||] => self.at_composite2(n, T![|], T![|]),

            T![...] => self.at_composite3(n, T![.], T![.], T![.]),
            T![..=] => self.at_composite3(n, T![.], T![.], T![=]),
            T![<<=] => self.at_composite3(n, T![<], T![<], T![=]),
            T![>>=] => self.at_composite3(n, T![>], T![>], T![=]),

            _ => self.inp.kind(self.pos + n) == kind,
        }
    }

    /// Consume the next token if `kind` matches.
    pub(crate) fn eat(&mut self, kind: SyntaxKind) -> bool {
        if !self.at(kind) {
            return false;
        }
        let n_raw_tokens = match kind {
            T![-=]
            | T![->]
            | T![::]
            | T![!=]
            | T![..]
            | T![*=]
            | T![/=]
            | T![&&]
            | T![&=]
            | T![%=]
            | T![^=]
            | T![+=]
            | T![<<]
            | T![<=]
            | T![==]
            | T![=>]
            | T![>=]
            | T![>>]
            | T![|=]
            | T![||] => 2,

            T![...] | T![..=] | T![<<=] | T![>>=] => 3,
            _ => 1,
        };
        self.do_bump(kind, n_raw_tokens);
        true
    }

    pub(crate) fn eat_contextual_kw(&mut self, kind: SyntaxKind) -> bool {
        if !self.at_contextual_kw(kind) {
            return false;
        }
        self.bump_remap(kind);
        true
    }

    /// This consumes the next token as an identifier, even if it is a strict keyword.
    pub(crate) fn bump_remap_any_ident(&mut self) {
        assert!(self.current().is_any_identifier());
        self.bump_remap(T![ident]);
    }

    fn at_composite2(&self, n: usize, k1: SyntaxKind, k2: SyntaxKind) -> bool {
        self.inp.kind(self.pos + n) == k1
            && self.inp.kind(self.pos + n + 1) == k2
            && self.inp.is_joint(self.pos + n)
    }

    fn at_composite3(&self, n: usize, k1: SyntaxKind, k2: SyntaxKind, k3: SyntaxKind) -> bool {
        self.inp.kind(self.pos + n) == k1
            && self.inp.kind(self.pos + n + 1) == k2
            && self.inp.kind(self.pos + n + 2) == k3
            && self.inp.is_joint(self.pos + n)
            && self.inp.is_joint(self.pos + n + 1)
    }

    /// Checks if the current token is in `kinds`.
    pub(crate) fn at_ts(&self, kinds: TokenSet) -> bool {
        kinds.contains(self.current())
    }

    /// Checks if the current token is contextual keyword `kw`.
    pub(crate) fn at_contextual_kw(&self, kw: SyntaxKind) -> bool {
        self.inp.contextual_kind(self.pos) == kw
    }

    /// Checks if the nth token is contextual keyword `kw`.
    pub(crate) fn nth_at_contextual_kw(&self, n: usize, kw: SyntaxKind) -> bool {
        self.inp.contextual_kind(self.pos + n) == kw
    }

    /// Starts a new node in the syntax tree. All nodes and tokens
    /// consumed between the `start` and the corresponding `Marker::complete`
    /// belong to the same node.
    pub(crate) fn start(&mut self) -> Marker {
        self.step();
        let pos = self.events.len() as u32;
        self.push_event(Event::tombstone());
        Marker::new(pos)
    }

    /// Consume the next token. Panics if the parser isn't currently at `kind`.
    pub(crate) fn bump(&mut self, kind: SyntaxKind) {
        assert!(self.eat(kind));
    }

    /// Advances the parser by one token
    pub(crate) fn bump_any(&mut self) {
        let kind = self.nth(0);
        if kind == EOF {
            self.step();
            return;
        }
        self.do_bump(kind, 1);
    }

    /// Advances the parser by one token
    pub(crate) fn split_float(&mut self, mut marker: Marker) -> (bool, Marker) {
        assert!(self.at(SyntaxKind::FLOAT_NUMBER));
        // we have parse `<something>.`
        // `<something>`.0.1
        // here we need to insert an extra event
        //
        // `<something>`. 0. 1;
        // here we need to change the follow up parse, the return value will cause us to emulate a dot
        // the actual splitting happens later
        let ends_in_dot = !self.inp.is_joint(self.pos);
        if !ends_in_dot {
            let new_marker = self.start();
            let idx = marker.pos as usize;
            match &mut self.events[idx] {
                Event::Start { forward_parent, kind } => {
                    *kind = SyntaxKind::FIELD_EXPR;
                    *forward_parent = Some(fwd_parent(new_marker.pos - marker.pos));
                }
                _ => unreachable!(),
            }
            marker.done = true;
            marker = new_marker;
        };
        self.pos += 1;
        self.steps = 0;
        self.push_event(Event::FloatSplitHack { ends_in_dot });
        (ends_in_dot, marker)
    }

    /// Advances the parser by one token, remapping its kind.
    /// This is useful to create contextual keywords from
    /// identifiers. For example, the lexer creates a `union`
    /// *identifier* token, but the parser remaps it to the
    /// `union` keyword, and keyword is what ends up in the
    /// final tree.
    pub(crate) fn bump_remap(&mut self, kind: SyntaxKind) {
        if self.nth(0) == EOF {
            // FIXME: panic!?
            self.step();
            return;
        }
        self.do_bump(kind, 1);
    }

    /// Emit error with the `message`
    /// FIXME: this should be much more fancy and support
    /// structured errors with spans and notes, like rustc
    /// does.
    ///
    /// A failed [`Parser::expect`] and every path of [`Parser::err_recover`] come through
    /// here, so each counts as a step.
    /// A fixed message is kept borrowed; only a formatted one is an owned `String`.
    pub(crate) fn error<T: Into<Cow<'static, str>>>(&mut self, message: T) {
        self.step();
        let err = self.errors.len() as u32;
        self.errors.push(message.into());
        self.push_event(Event::Error { err });
    }

    /// Consume the next token if it is `kind` or emit an error
    /// otherwise.
    pub(crate) fn expect(&mut self, kind: SyntaxKind) -> bool {
        if self.eat(kind) {
            return true;
        }
        self.error(format!("expected {kind:?}")); // alloc: a syntax error, an error path
        false
    }

    /// Create an error node and consume the next token.
    /// A `{` some `}` closes is taken with everything up to that `}`, each pair inside it in a
    /// node of its own, so every pair of braces is the first and last child of one node.
    pub(crate) fn err_and_bump(&mut self, message: &'static str) {
        let m = self.start();
        self.error(message);
        if self.at(T!['{']) && self.curly_closes(self.pos) {
            // Every `{` after a closed one is closed before it: a stack pairs them.
            let mut inner = Vec::new(); // alloc: an error path, one marker a nested pair
            self.bump_any();
            loop {
                match self.current() {
                    T!['{'] => {
                        inner.push(self.start());
                        self.bump_any();
                    }
                    T!['}'] => {
                        self.bump_any();
                        match inner.pop() {
                            Some(pair) => {
                                pair.complete(self, ERROR);
                            }
                            None => break,
                        }
                    }
                    _ => self.bump_any(),
                }
            }
        } else {
            self.bump_any();
        }
        m.complete(self, ERROR);
    }

    /// Create an error node and consume the next token unless it is in the recovery set.
    ///
    /// Returns true if recovery kicked in.
    pub(crate) fn err_recover(&mut self, message: &'static str, recovery: TokenSet) -> bool {
        if matches!(self.current(), T!['{'] | T!['}']) {
            self.error(message);
            return true;
        }

        if self.at_ts(recovery) {
            self.error(message);
            return true;
        }

        let m = self.start();
        self.error(message);
        self.bump_any();
        m.complete(self, ERROR);
        false
    }

    fn do_bump(&mut self, kind: SyntaxKind, n_raw_tokens: u8) {
        self.pos += n_raw_tokens as usize;
        debug_assert!(self.pos <= self.inp.len(), "a bump past the end of the input");
        self.steps = 0;
        self.push_event(Event::Token { kind, n_raw_tokens });
    }

    fn push_event(&mut self, event: Event) {
        self.events.push(event);
    }

    pub(crate) fn current_edition(&self) -> Edition {
        self.inp.edition(self.pos)
    }
}

/// See [`Parser::start`].
pub(crate) struct Marker {
    pos: u32,
    /// Completed, abandoned or taken by `extend_to` or `split_float`: dropping it is fine.
    done: bool,
}

impl Drop for Marker {
    #[inline]
    fn drop(&mut self) {
        if !self.done && !std::thread::panicking() {
            panic!("Marker must be either completed or abandoned");
        }
    }
}

impl Marker {
    fn new(pos: u32) -> Marker {
        Marker { pos, done: false }
    }

    /// Finishes the syntax tree node and assigns `kind` to it,
    /// and mark the create a `CompletedMarker` for possible future
    /// operation like `.precede()` to deal with forward_parent.
    pub(crate) fn complete(mut self, p: &mut Parser<'_>, kind: SyntaxKind) -> CompletedMarker {
        self.done = true;
        let idx = self.pos as usize;
        match &mut p.events[idx] {
            Event::Start { kind: slot, .. } => {
                *slot = kind;
            }
            _ => unreachable!(),
        }
        p.push_event(Event::Finish);
        let end_pos = p.events.len() as u32;
        CompletedMarker::new(self.pos, end_pos, kind)
    }

    /// Abandons the syntax tree node. All its children
    /// are attached to its parent instead.
    pub(crate) fn abandon(mut self, p: &mut Parser<'_>) {
        self.done = true;
        let idx = self.pos as usize;
        if idx == p.events.len() - 1 {
            assert!(matches!(
                p.events.pop(),
                Some(Event::Start { kind: TOMBSTONE, forward_parent: None })
            ));
        }
    }
}

pub(crate) struct CompletedMarker {
    start_pos: u32,
    end_pos: u32,
    kind: SyntaxKind,
}

impl CompletedMarker {
    fn new(start_pos: u32, end_pos: u32, kind: SyntaxKind) -> Self {
        CompletedMarker { start_pos, end_pos, kind }
    }

    /// This method allows to create a new node which starts
    /// *before* the current one. That is, parser could start
    /// node `A`, then complete it, and then after parsing the
    /// whole `A`, decide that it should have started some node
    /// `B` before starting `A`. `precede` allows to do exactly
    /// that. See also docs about
    /// [`Event::Start::forward_parent`](crate::event::Event::Start::forward_parent).
    ///
    /// Given completed events `[START, FINISH]` and its corresponding
    /// `CompletedMarker(pos: 0, _)`.
    /// Append a new `START` events as `[START, FINISH, NEWSTART]`,
    /// then mark `NEWSTART` as `START`'s parent with saving its relative
    /// distance to `NEWSTART` into forward_parent(=2 in this case);
    pub(crate) fn precede(self, p: &mut Parser<'_>) -> Marker {
        let new_pos = p.start();
        let idx = self.start_pos as usize;
        match &mut p.events[idx] {
            Event::Start { forward_parent, .. } => {
                *forward_parent = Some(fwd_parent(new_pos.pos - self.start_pos));
            }
            _ => unreachable!(),
        }
        new_pos
    }

    /// Extends this completed marker *to the left* up to `m`.
    pub(crate) fn extend_to(self, p: &mut Parser<'_>, mut m: Marker) -> CompletedMarker {
        m.done = true;
        let idx = m.pos as usize;
        match &mut p.events[idx] {
            Event::Start { forward_parent, .. } => {
                *forward_parent = Some(fwd_parent(self.start_pos - m.pos));
            }
            _ => unreachable!(),
        }
        self
    }

    pub(crate) fn kind(&self) -> SyntaxKind {
        self.kind
    }

    pub(crate) fn last_token(&self, p: &Parser<'_>) -> Option<SyntaxKind> {
        let end_pos = self.end_pos as usize;
        debug_assert_eq!(p.events[end_pos - 1], Event::Finish);
        p.events[..end_pos].iter().rev().find_map(|event| match event {
            Event::Token { kind, .. } => Some(*kind),
            // A lazy node is a balanced `{ ... }`.
            Event::Lazy { .. } => Some(T!['}']),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Input;

    fn input(kinds: &[SyntaxKind]) -> Input {
        let mut input = Input::with_capacity(kinds.len());
        for &kind in kinds {
            input.push(kind, Edition::CURRENT);
        }
        input
    }

    #[test]
    #[should_panic(expected = "Marker must be either completed or abandoned")]
    fn a_marker_dropped_unfinished_panics() {
        let input = input(&[T![;]]);
        let mut p = Parser::new(&input);
        let _m = p.start();
    }

    #[test]
    fn a_finished_marker_drops_quietly() {
        let input = input(&[T![;], T![;]]);
        let mut p = Parser::new(&input);
        let m = p.start();
        p.bump(T![;]);
        let done = m.complete(&mut p, ERROR);
        p.start().abandon(&mut p);
        let m = done.precede(&mut p);
        let outer = p.start();
        p.bump(T![;]);
        outer.complete(&mut p, ERROR).extend_to(&mut p, m);
        assert_eq!(p.finish().0.len(), 7);
    }

    #[test]
    fn kind_tests_are_not_steps() {
        let input = input(&[T![;]]);
        let p = Parser::new(&input);
        for _ in 0..=PARSER_STEP_LIMIT {
            assert_eq!(p.current(), T![;]);
            assert!(p.nth_at(1, EOF));
        }
    }

    #[test]
    #[should_panic(expected = "the parser seems stuck")]
    fn starting_nodes_without_consuming_a_token_is_stuck() {
        let input = input(&[T![;]]);
        let mut p = Parser::new(&input);
        loop {
            p.start().abandon(&mut p);
        }
    }

    #[test]
    #[should_panic(expected = "the parser seems stuck")]
    fn bumping_at_eof_is_stuck() {
        let input = input(&[]);
        let mut p = Parser::new(&input);
        loop {
            p.bump_any();
        }
    }

    #[test]
    #[should_panic(expected = "the parser seems stuck")]
    fn failing_to_expect_is_stuck() {
        let input = input(&[T![;]]);
        let mut p = Parser::new(&input);
        p.steps = PARSER_STEP_LIMIT as u32 - 1;
        assert!(!p.expect(T![,]));
        p.expect(T![,]);
    }

    #[test]
    #[should_panic(expected = "the parser seems stuck")]
    fn recovering_at_the_recovery_set_is_stuck() {
        let input = input(&[T![;]]);
        let mut p = Parser::new(&input);
        p.steps = PARSER_STEP_LIMIT as u32 - 1;
        assert!(p.err_recover("x", TokenSet::new(&[T![;]])));
        p.err_recover("x", TokenSet::new(&[T![;]]));
    }

    #[test]
    fn a_consumed_token_resets_the_count() {
        let input = input(&[T![;], T![;]]);
        let mut p = Parser::new(&input);
        for _ in 0..2 {
            for _ in 1..PARSER_STEP_LIMIT {
                p.start().abandon(&mut p);
            }
            p.bump(T![;]);
        }
        assert_eq!(p.current(), EOF);
    }
}
