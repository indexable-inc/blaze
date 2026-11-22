//! Where the grammar's nodes go: the one seam between the grammar and what is made of a parse.
//!
//! The grammar opens a node before it knows the node's kind, may give it up
//! ([`Sink::abandon`]) and may wrap a finished node in a new one that starts where it did
//! ([`Sink::precede`]). A [`Sink`] is told each of those steps as it happens, with the index of
//! the next input token, and resolves them its own way: [`EventSink`] keeps the flat list of
//! events the tree builder reads once the parse is done; a sink that needs only what the
//! nodes say (an outline) keeps a stack and nothing else. The grammar is the same for every
//! sink (`Parser<'t, S>`): a strategy at one seam, each instantiation tested equal.
//!
//! Handles are `u32`s whose meaning is the sink's own: one for an open node ([`Sink::start`]),
//! and for a finished node its start and its end ([`Done`]).

use std::{borrow::Cow, num::NonZeroU32};

use crate::{
    SyntaxKind::{self, TOMBSTONE},
    T,
    event::{self, Event, Events},
    input::Tokens,
};

/// A syntax error's message: a fixed one is borrowed, a formatted one owned.
pub type Message = Cow<'static, str>;

/// A finished node's two handles, as its sink made them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Done {
    /// What [`Sink::precede`] and [`Sink::extend_to`] take.
    pub start: u32,
    /// What [`Sink::last_token`] takes.
    pub end: u32,
}

/// What the grammar tells of a parse, step by step. `at` is always the index of the next input
/// token not yet taken.
pub trait Sink {
    /// A node opens. Its kind comes when it is finished ([`Sink::complete`]), if it is.
    fn start(&mut self, at: u32) -> u32;

    /// A node opens that will be finished as `kind`, never abandoned and never preceded: one
    /// of the lists whose children are items, variants or fields, or a source file's root. A
    /// sink that reads items needs to know a list before its first child opens.
    fn start_list(&mut self, kind: SyntaxKind, at: u32) -> u32 {
        let _ = kind;
        self.start(at)
    }

    /// Whether the sink has all it reads of this parse, so that the rest need not be parsed:
    /// asked where a block's next statement would start, and when true no further statement of
    /// that block is read, nor of the blocks around it, each asked in turn as its statement
    /// ends. What the sink is told after it first answers true is of a parse cut short (nodes
    /// closed early, errors for what was not read): a sink that answers true reads none of it,
    /// and its answer never turns false again. A sink that reads the whole parse (the tree
    /// builder's events, an outline's rows) never does: the default.
    #[inline]
    fn satisfied(&self) -> bool {
        false
    }

    /// The next `n_raw_tokens` input tokens are one token of `kind`.
    fn token(&mut self, kind: SyntaxKind, n_raw_tokens: u8);

    /// The open node `mark` is finished as `kind`.
    fn complete(&mut self, mark: u32, kind: SyntaxKind, at: u32) -> Done;

    /// The open node `mark` is given up: what it holds is its parent's.
    fn abandon(&mut self, mark: u32, at: u32);

    /// A node opens that starts where the finished node `done` (a [`Done::start`]) started and
    /// holds it and everything after it.
    fn precede(&mut self, done: u32, at: u32) -> u32;

    /// The finished node `done`, the last thing in the open node `mark`, takes `mark`'s place:
    /// it starts where `mark` did and holds what `mark` held before it.
    fn extend_to(&mut self, done: u32, mark: u32) -> u32;

    /// The next input token is a float literal after a `.`: `x.0.1` or `x.0.`. The open node
    /// `mark` is a field expression ending inside the literal. When the literal does not end
    /// in a dot, `mark` is finished as a `FIELD_EXPR` and the handle returned is a new open
    /// node around it, which the grammar finishes next; else the handle is `mark`'s.
    fn split_float(&mut self, mark: u32, ends_in_dot: bool, at: u32) -> u32;

    /// A syntax error before token `at`.
    fn error(&mut self, message: Message, at: u32);

    /// The next `n_input_tokens` tokens are a node of `kind` left unparsed (an outline's `fn`
    /// body).
    fn lazy(&mut self, kind: SyntaxKind, n_input_tokens: u32, at: u32);

    /// The kind of the last token of the finished node ending at `end` (a [`Done::end`]).
    fn last_token(&self, end: u32, input: Tokens<'_>) -> Option<SyntaxKind>;
}

/// Build a forward-parent offset. The offset is always ≥ 1 because the
/// forward-parent event is created *after* the event it forwards to, so
/// `NonZeroU32` is always valid here. Panics only on a parser bug.
#[inline]
fn fwd_parent(offset: u32) -> NonZeroU32 {
    NonZeroU32::new(offset).expect("forward-parent offset must be non-zero")
}

/// The sink of a parse whose tree is built: the flat list of events ([`Event`]) that
/// [`event::process`] turns into an [`crate::Output`]. A handle is an event's index.
pub struct EventSink {
    /// The thread's event list, lent until [`event::process`] is done with it.
    pub(crate) events: blaze_mem::scratch::Lent<Events>,
    /// Side table of error messages. `Event::Error { err }` carries an index
    /// into this vec, keeping `Event` itself a flat 8-byte enum.
    pub(crate) errors: Vec<Cow<'static, str>>,
}

impl EventSink {
    /// An empty list with room for the events of `tokens` tokens.
    pub(crate) fn for_tokens(tokens: usize) -> EventSink {
        EventSink {
            events: event::lend_events(tokens),
            errors: Vec::new(), // alloc: empty until the first syntax error, an error path
        }
    }

    fn start_event(&mut self, at: usize) -> (&mut SyntaxKind, &mut Option<NonZeroU32>) {
        match &mut self.events[at] {
            Event::Start { kind, forward_parent } => (kind, forward_parent),
            _ => unreachable!(),
        }
    }
}

impl Sink for EventSink {
    #[inline]
    fn start(&mut self, _at: u32) -> u32 {
        let pos = self.events.len() as u32;
        self.events.push(Event::tombstone());
        pos
    }

    #[inline]
    fn token(&mut self, kind: SyntaxKind, n_raw_tokens: u8) {
        self.events.push(Event::Token { kind, n_raw_tokens });
    }

    #[inline]
    fn complete(&mut self, mark: u32, kind: SyntaxKind, _at: u32) -> Done {
        *self.start_event(mark as usize).0 = kind;
        self.events.push(Event::Finish);
        Done { start: mark, end: self.events.len() as u32 }
    }

    fn abandon(&mut self, mark: u32, _at: u32) { if mark as usize == self.events.len() - 1 {
        assert!(matches!(
            self.events.pop(),
            Some(Event::Start { kind: TOMBSTONE, forward_parent: None })
        ));
    } }

    #[inline]
    fn precede(&mut self, done: u32, at: u32) -> u32 {
        let new = self.start(at);
        *self.start_event(done as usize).1 = Some(fwd_parent(new - done));
        new
    }

    fn extend_to(&mut self, done: u32, mark: u32) -> u32 {
            *self.start_event(mark as usize).1 = Some(fwd_parent(done - mark));
            done
        }

    fn split_float(&mut self, mark: u32, ends_in_dot: bool, at: u32) -> u32 {
        // we have parse `<something>.`
        // `<something>`.0.1
        // here we need to insert an extra event
        //
        // `<something>`. 0. 1;
        // here we need to change the follow up parse, the return value will cause us to emulate a dot
        // the actual splitting happens later
        let mut open = mark;
        if !ends_in_dot {
            let new = self.start(at);
            let (kind, forward_parent) = self.start_event(mark as usize);
            *kind = SyntaxKind::FIELD_EXPR;
            *forward_parent = Some(fwd_parent(new - mark));
            open = new;
        }
        self.events.push(Event::FloatSplitHack { ends_in_dot });
        open
    }

    fn error(&mut self, message: Cow<'static, str>, _at: u32) {
        let err = self.errors.len() as u32;
        self.errors.push(message);
        self.events.push(Event::Error { err });
    }

    fn lazy(&mut self, kind: SyntaxKind, n_input_tokens: u32, _at: u32) {
        self.events.push(Event::Lazy { kind, n_input_tokens });
    }

    fn last_token(&self, end: u32, _input: Tokens<'_>) -> Option<SyntaxKind> {
        let end = end as usize;
        debug_assert_eq!(self.events[end - 1], Event::Finish);
        self.events[..end].iter().rev().find_map(|event| match event {
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
    use crate::{Edition, LexedStr, SyntaxKind::*};

    /// The kinds of the nodes a block's parse finishes, in order; with `stop`, satisfied once a
    /// `MACRO_CALL` is finished.
    struct Kinds {
        stop: bool,
        seen: Vec<SyntaxKind>,
    }

    impl Sink for Kinds {
        fn satisfied(&self) -> bool {
            self.stop && self.seen.contains(&MACRO_CALL)
        }
        fn start(&mut self, _at: u32) -> u32 {
            0
        }
        fn token(&mut self, _kind: SyntaxKind, _n_raw_tokens: u8) {}
        fn complete(&mut self, _mark: u32, kind: SyntaxKind, at: u32) -> Done {
            self.seen.push(kind);
            Done { start: 0, end: at }
        }
        fn abandon(&mut self, _mark: u32, _at: u32) {}
        fn precede(&mut self, _done: u32, _at: u32) -> u32 {
            0
        }
        fn extend_to(&mut self, done: u32, _mark: u32) -> u32 {
            done
        }
        fn split_float(&mut self, mark: u32, _ends_in_dot: bool, _at: u32) -> u32 {
            mark
        }
        fn error(&mut self, _message: Message, _at: u32) {}
        fn lazy(&mut self, kind: SyntaxKind, _n_input_tokens: u32, _at: u32) {
            unreachable!("a block's parse leaves no lazy {kind:?}")
        }
        fn last_token(&self, end: u32, input: Tokens<'_>) -> Option<SyntaxKind> {
            end.checked_sub(1).map(|at| input.kind(at as usize))
        }
    }

    fn kinds(text: &str, stop: bool) -> Vec<SyntaxKind> {
        let input = LexedStr::new(Edition::CURRENT, text).to_input(Edition::CURRENT);
        crate::block_into(input.tokens(), Kinds { stop, seen: Vec::new() }).seen
    }

    /// A satisfied sink ends the parse at the next statement boundary, in the block the
    /// statement is in and in each block around it: what comes after is not parsed (no
    /// `CALL_EXPR`, no `LET_STMT`, no second `MACRO_CALL`), every node open then is still
    /// finished, and what was finished before the sink was satisfied is what a whole parse
    /// finishes first. Without the answer the same grammar parses it all.
    #[test]
    fn a_satisfied_sink_ends_the_parse_at_the_next_statement() {
        let text = "{ a; if b { c!(); d(); let e = 1; } f(); g!(); }";
        let whole = kinds(text, false);
        assert_eq!(whole.iter().filter(|&&kind| kind == MACRO_CALL).count(), 2);
        assert!(whole.contains(&CALL_EXPR) && whole.contains(&LET_STMT));
        let cut = kinds(text, true);
        assert_eq!(cut.iter().filter(|&&kind| kind == MACRO_CALL).count(), 1);
        assert!(!cut.contains(&CALL_EXPR) && !cut.contains(&LET_STMT), "{cut:?}");
        let upto = cut.iter().position(|&kind| kind == MACRO_CALL).expect("the call") + 1;
        assert_eq!(cut[..upto], whole[..upto], "the same parse up to the call");
        assert_eq!(cut.last(), Some(&BLOCK_EXPR), "the block itself is finished: {cut:?}");
    }
}
