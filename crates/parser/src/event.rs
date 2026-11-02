//! This module provides a way to construct a `File`.
//! It is intended to be completely decoupled from the
//! parser, so as to allow to evolve the tree representation
//! and the parser algorithm independently.
use std::{borrow::Cow, mem, num::NonZeroU32};

use blaze_mem::scratch;

use crate::{
    SyntaxKind::{self, *},
    output::Output,
};

/// `Parser` produces a flat list of `Event`s.
/// They are converted to a tree-structure in
/// a separate pass, via `TreeBuilder`.
///
/// Kept to 8 bytes: error messages live in a side table on the `Parser`
/// (the `errors` vec) and `Event::Error` only stores an index into it.
/// `forward_parent` uses `NonZeroU32` so `Option` is niche-optimised away
/// (the offset is always ≥ 1 because the forward parent sits later in the
/// event stream).
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) enum Event {
    /// This event signifies the start of the node.
    /// It should be either abandoned (in which case the
    /// `kind` is `TOMBSTONE`, and the event is ignored),
    /// or completed via a `Finish` event.
    ///
    /// All tokens between a `Start` and a `Finish` would
    /// become the children of the respective node.
    ///
    /// For left-recursive syntactic constructs, the parser produces
    /// a child node before it sees a parent. `forward_parent`
    /// saves the position of current event's parent.
    ///
    /// Consider this path
    ///
    /// foo::bar
    ///
    /// The events for it would look like this:
    ///
    /// ```text
    /// START(PATH) IDENT('foo') FINISH START(PATH) T![::] IDENT('bar') FINISH
    ///       |                          /\
    ///       |                          |
    ///       +------forward-parent------+
    /// ```
    ///
    /// And the tree would look like this
    ///
    /// ```text
    ///    +--PATH---------+
    ///    |   |           |
    ///    |   |           |
    ///    |  '::'       'bar'
    ///    |
    ///   PATH
    ///    |
    ///   'foo'
    /// ```
    ///
    /// See also `CompletedMarker::precede`.
    Start { kind: SyntaxKind, forward_parent: Option<NonZeroU32> },

    /// Complete the previous `Start` event
    Finish,

    /// Produce a single leaf-element.
    /// `n_raw_tokens` is used to glue complex contextual tokens.
    /// For example, lexer tokenizes `>>` as `>`, `>`, and
    /// `n_raw_tokens = 2` is used to produced a single `>>`.
    Token { kind: SyntaxKind, n_raw_tokens: u8 },
    /// When we parse `foo.0.0` or `foo. 0. 0` the lexer will hand us a float literal
    /// instead of an integer literal followed by a dot as the lexer has no contextual knowledge.
    /// This event instructs whatever consumes the events to split the float literal into
    /// the corresponding parts.
    FloatSplitHack { ends_in_dot: bool },
    /// Index into the parser's side `errors` vec.
    Error { err: u32 },
    /// A node of `kind` an outline leaves unparsed, over the next `n_input_tokens` tokens.
    Lazy { kind: SyntaxKind, n_input_tokens: u32 },
}

const _: () = assert!(size_of::<Event>() == 8, "an event is 8 bytes");

impl Event {
    pub(crate) fn tombstone() -> Self {
        Event::Start { kind: TOMBSTONE, forward_parent: None }
    }
}

/// A buffer a parse needs only while it runs, kept per thread at the most it ever held
/// (`blaze_mem::scratch`, docs/v2/plan/design/memory.md): `scratch::lend` or `scratch::with`
/// hands out the thread's one, empty, and takes it back when done. A parse inside a parse on
/// the same thread gets a new one, and of two coming back the larger is kept.
macro_rules! per_thread_buffer {
    ($(#[$doc:meta])* $name:ident, $shelf:ident: $item:ty) => {
        $(#[$doc])*
        #[derive(Default)]
        pub(crate) struct $name(Vec<$item>);

        ::std::thread_local! {
            static $shelf: ::std::cell::Cell<Vec<$item>> =
                const { ::std::cell::Cell::new(Vec::new()) }; // alloc: none, empty
        }

        impl ::std::ops::Deref for $name {
            type Target = Vec<$item>;

            #[inline]
            fn deref(&self) -> &Vec<$item> {
                &self.0
            }
        }

        impl ::std::ops::DerefMut for $name {
            #[inline]
            fn deref_mut(&mut self) -> &mut Vec<$item> {
                &mut self.0
            }
        }

        impl ::blaze_mem::scratch::Shelved for $name {
            #[inline]
            fn empty(&mut self) {
                self.0.clear();
            }

            #[inline]
            fn held(&self) -> usize {
                self.0.capacity()
            }

            #[inline]
            fn unshelve() -> Option<Self> {
                let buffer = $shelf.try_with(::std::cell::Cell::take).ok()?;
                (buffer.capacity() != 0).then_some($name(buffer))
            }

            #[inline]
            fn shelve(self) {
                // A thread that is ending: the buffer is freed.
                let _ = $shelf.try_with(|shelf| {
                    let kept = shelf.take();
                    shelf.set(if kept.capacity() >= self.0.capacity() { kept } else { self.0 });
                });
            }
        }
    };
}

pub(crate) use per_thread_buffer;

per_thread_buffer! {
    /// The parser's event list: it lives from `Parser::new` to the end of [`process`].
    Events, EVENTS: Event
}

per_thread_buffer! {
    /// [`process`]'s chain of forward parents, as long as the longest `precede` chain.
    ForwardParents, FORWARD_PARENTS: SyntaxKind
}

/// The thread's event list, empty, with room for `tokens` tokens' events: one `Token` each,
/// and a `Start` and a `Finish` for each of the 1.05 nodes a token, about 3.1 events, so a
/// cold list sized at 3.25 seldom doubles. Warm, this allocates nothing.
pub(crate) fn lend_events(tokens: usize) -> scratch::Lent<Events> {
    let mut events = scratch::lend::<Events>();
    events.reserve((tokens * 13).div_ceil(4));
    events
}

/// Generate the syntax tree with the control of events. `errors` is the
/// side table of error messages built up alongside the `events` stream.
pub(super) fn process(
    mut events: scratch::Lent<Events>,
    errors: Vec<Cow<'static, str>>,
) -> Output {
    scratch::with::<ForwardParents, _>(|forward_parents| {
        process_with(&mut events, errors, forward_parents)
    })
}

fn process_with(
    events: &mut [Event],
    mut errors: Vec<Cow<'static, str>>,
    forward_parents: &mut Vec<SyntaxKind>,
) -> Output {
    // Each event becomes roughly one u32 in Output, so preallocate to avoid
    // the amortized grow-one churn we used to see in Output::enter_node.
    let mut res = Output::with_event_capacity(events.len());

    for i in 0..events.len() {
        match events[i] {
            // Most nodes have no forward parent: they enter at once, with no list to reverse.
            Event::Start { kind, forward_parent: None } => {
                if kind != TOMBSTONE {
                    res.enter_node(kind);
                }
            }
            Event::Start { kind, forward_parent } => {
                // For events[A, B, C], B is A's forward_parent, C is B's forward_parent,
                // in the normal control flow, the parent-child relation: `A -> B -> C`,
                // while with the magic forward_parent, it writes: `C <- B <- A`.

                // append `A` into parents.
                forward_parents.push(kind);
                let mut idx = i;
                let mut fp = forward_parent;
                while let Some(fwd) = fp {
                    idx += fwd.get() as usize;
                    // append `A`'s forward_parent `B`
                    fp = match mem::replace(&mut events[idx], Event::tombstone()) {
                        Event::Start { kind, forward_parent } => {
                            forward_parents.push(kind);
                            forward_parent
                        }
                        _ => unreachable!(),
                    };
                    // append `B`'s forward_parent `C` in the next stage.
                }

                for kind in forward_parents.drain(..).rev() {
                    if kind != TOMBSTONE {
                        res.enter_node(kind);
                    }
                }
            }
            Event::Finish => res.leave_node(),
            Event::Token { kind, n_raw_tokens } => {
                res.token(kind, n_raw_tokens);
            }
            Event::FloatSplitHack { ends_in_dot } => {
                res.float_split_hack(ends_in_dot);
                let ev = mem::replace(&mut events[i + 1], Event::tombstone());
                assert!(matches!(ev, Event::Finish), "{ev:?}");
            }
            Event::Error { err } => {
                // Move the message out of the side table; each index is visited
                // exactly once, so swapping with an empty one is cheap and
                // avoids any clone.
                let msg = mem::take(&mut errors[err as usize]);
                res.error(msg);
            }
            Event::Lazy { kind, n_input_tokens } => res.lazy(kind, n_input_tokens),
        }
    }

    res
}
