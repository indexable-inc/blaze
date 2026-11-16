//! The parser's steps to the machine's events: each step's tokens found in the lexer's
//! (trivia skipped), given as one token of the step's kind over their range, shifted by
//! `base` (where the parsed text starts in the item), so every range is the item's.

use blaze_surface::Range;
use parser::{LexedStr, OutlineStep, Output, Step, SyntaxKind};

use crate::machine::Machine;

/// Where the feed is in the lexer's tokens.
struct Cursor<'l, 't> {
    lexed: &'l LexedStr<'t>,
    pos: usize,
    offset: u32,
    base: u32,
}

impl Cursor<'_, '_> {
    fn len(&self, at: usize) -> u32 {
        u32::try_from(self.lexed.text_len(at)).expect("a token of an item under 4 GiB")
    }

    fn skip_trivia(&mut self) {
        while self.pos < self.lexed.len() && self.lexed.kind(self.pos).is_trivia() {
            self.offset += self.len(self.pos);
            self.pos += 1;
        }
    }

    /// Takes `n` input tokens (a composite token is several, joint): their range.
    fn take(&mut self, n: u32) -> Range {
        self.skip_trivia();
        let start = self.offset;
        for i in 0..n {
            if i > 0 {
                self.skip_trivia();
            }
            assert!(self.pos < self.lexed.len(), "the parser took a token past the lexer's last");
            self.offset += self.len(self.pos);
            self.pos += 1;
        }
        Range::new(self.base + start, self.base + self.offset)
    }
}

/// Feeds `output`, the parse of `lexed`, to `m`.
pub(crate) fn feed(m: &mut Machine<'_, '_, '_>, lexed: &LexedStr<'_>, output: &Output, base: u32) {
    feed_steps(m, lexed, output.steps(), base);
}

/// Feeds `steps`, a parse's steps of `lexed`, to `m`.
pub(crate) fn feed_steps<'s>(
    m: &mut Machine<'_, '_, '_>,
    lexed: &LexedStr<'_>,
    steps: impl Iterator<Item = OutlineStep<'s>>,
    base: u32,
) {
    assert!(u32::try_from(lexed.as_str().len()).is_ok(), "an item of over 4 GiB: ranges are u32 pairs");
    let mut at = Cursor { lexed, pos: 0, offset: 0, base };
    for step in steps {
        match step {
            OutlineStep::Step(Step::Token { kind, n_input_tokens }) => {
                let range = at.take(u32::from(n_input_tokens));
                m.token(kind, range);
            }
            // `x.0.1`: one float token, split into the inner field's name, the inner field's
            // exit, the dot and, past a second field, the outer field's name and exit.
            OutlineStep::Step(Step::FloatSplit { ends_in_dot }) => {
                let whole = at.take(1);
                let text = whole.of(m.text());
                let left = text.find('.').map_or(text.len(), |i| i) as u32;
                let dot = Range::new(whole.start + left, (whole.start + left + 1).min(whole.end));
                m.enter(SyntaxKind::NAME_REF);
                m.token(SyntaxKind::INT_NUMBER, Range::new(whole.start, dot.start));
                m.exit();
                m.exit();
                m.token(SyntaxKind::DOT, dot);
                if !ends_in_dot {
                    m.enter(SyntaxKind::NAME_REF);
                    m.token(SyntaxKind::INT_NUMBER, Range::new(dot.end, whole.end));
                    m.exit();
                    m.exit();
                }
            }
            OutlineStep::Step(Step::Enter { kind }) => m.enter(kind),
            OutlineStep::Step(Step::Exit) => m.exit(),
            OutlineStep::Step(Step::Error { .. }) => m.parse_error(),
            OutlineStep::Lazy { kind, n_input_tokens } => {
                let range = at.take(n_input_tokens);
                m.lazy(kind, range);
            }
        }
    }
}
