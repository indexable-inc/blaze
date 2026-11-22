//! The parser's steps to the machine's events: each step's tokens found in the lexer's
//! (trivia skipped), given as one token of the step's kind over their range, shifted by
//! `base` (where the parsed text starts in the item), so every range is the item's. A body's
//! macro call, when the feed has an expander, is expanded where it closes and its
//! expansion's parse fed next, its words' ranges those [`Machine::append_expansion`] gave.

use blaze_own::{AVec, Vec};
use blaze_surface::Range;
use parser::{LexedStr, OutlineStep, Output, Step, SyntaxKind};

use crate::driver::{BodyCall, BodyOut, EXPANSION_BUDGET, EXPANSION_DEPTH, Expand, Expansion, LocalMacro};
use crate::form::Why;
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

/// Feeds `output`, the parse of `lexed`, to `m`. A macro call met is kept as a macro call
/// node: this feed has no expander (an expanding feed's own nested ones, `format_args!`'s
/// arguments, are not expanded either).
pub(crate) fn feed(m: &mut Machine<'_, '_, '_>, lexed: &LexedStr<'_>, output: &Output, base: u32) {
    run(m, output, lexed_toks(lexed, base), None);
}

/// The lexer's tokens of `lexed` from its first, their ranges shifted by `base`.
fn lexed_toks<'l, 't>(lexed: &'l LexedStr<'t>, base: u32) -> Toks<'l, 't, 'static> {
    assert!(u32::try_from(lexed.as_str().len()).is_ok(), "an item of over 4 GiB: ranges are u32 pairs");
    Toks::Lexed(Cursor { lexed, pos: 0, offset: 0, base })
}

/// Feeds `output`, the parse of an item macro's expansion read as words (`ranges`, each
/// word's range in the machine's text), to `m`, each body macro call in it expanded through
/// `expand` when there is one: the expansion's `$crate` and its other words are read as the
/// expander wrote them, never lexed again from text.
pub(crate) fn feed_words(m: &mut Machine<'_, '_, '_>, output: &Output, ranges: &[Range], expand: Option<&mut dyn Expand>) {
    let mut owned = Vec::with_capacity_in(ranges.len(), m.allocator());
    owned.extend_from_slice(ranges);
    let expanding = expand.is_some();
    m.expanding = expanding;
    run(m, output, Toks::Words { ranges: owned, pos: 0 }, expand);
    m.expanding = false;
}

/// Feeds `output`, the parse of a text's input words, to `m`: `ranges` is each word's range in
/// the text (the column [`parser::LexedStr::each_input_token`] gives beside the words), `end` the
/// text's length. A step takes its range from the column in one step, never walking the trivia
/// between its tokens; what it takes is [`Cursor::take`]'s on the same text (a take of no token
/// is where the next token starts, or the text's end past the last).
pub(crate) fn feed_input<'a>(m: &mut Machine<'_, '_, 'a>, output: &Output, ranges: AVec<'a, Range>, end: u32) {
    run(m, output, Toks::Input { ranges, pos: 0, end }, None);
}

/// Feeds `output`, the parse of `lexed`, to `m`, expanding each body macro call through
/// `expand` and feeding its expansion's parse in place: one stack of the parses open, the
/// item's at the bottom, never a recursion.
pub(crate) fn feed_expanding(
    m: &mut Machine<'_, '_, '_>,
    lexed: &LexedStr<'_>,
    output: &Output,
    base: u32,
    expand: &mut dyn Expand,
) {
    m.expanding = true;
    run(m, output, lexed_toks(lexed, base), Some(expand));
    m.expanding = false;
}

/// The tokens a parse's steps take: the lexer's of a text, or an expansion's words.
enum Toks<'l, 't, 'a> {
    Lexed(Cursor<'l, 't>),
    /// Each word's range in the machine's text, and the next word.
    Words { ranges: AVec<'a, Range>, pos: usize },
    /// A text's input words' ranges ([`feed_input`]), the next word, and the text's length.
    Input { ranges: AVec<'a, Range>, pos: usize, end: u32 },
}

impl Toks<'_, '_, '_> {
    fn take(&mut self, n: u32) -> Range {
        match self {
            Toks::Lexed(cursor) => cursor.take(n),
            Toks::Words { ranges, pos } => {
                let n = n as usize;
                assert!(*pos + n <= ranges.len(), "the parser took a word past an expansion's last");
                let range = match n {
                    0 => ranges.get(*pos).map_or(Range::new(0, 0), |r| Range::new(r.start, r.start)),
                    _ => Range::new(ranges[*pos].start, ranges[*pos + n - 1].end),
                };
                *pos += n;
                range
            }
            Toks::Input { ranges, pos, end } => {
                let n = n as usize;
                assert!(*pos + n <= ranges.len(), "the parser took a token past the lexer's last");
                let range = match n {
                    0 => ranges.get(*pos).map_or(Range::new(*end, *end), |r| Range::new(r.start, r.start)),
                    _ => Range::new(ranges[*pos].start, ranges[*pos + n - 1].end),
                };
                *pos += n;
                range
            }
        }
    }
}

/// A parse being fed: the item's (borrowed) or an expansion's (owned).
enum Parse<'o> {
    Base(&'o Output),
    Expansion(Output),
}

struct Level<'o, 'l, 't, 'a> {
    parse: Parse<'o>,
    at: usize,
    toks: Toks<'l, 't, 'a>,
}

/// What one parse's steps need of the machine's two stacks, counted from its event words before
/// the first step, so neither grows by reallocation while the parse is fed (lane RP4, floor C3).
/// An expansion fed later is a parse of its own, its values on top: they may grow then.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Sizes {
    /// The most values live at once: one per token, lazy node and node (a node's exit leaves
    /// one in place of its children's), five more per float split (`x.0.1`: two names, their
    /// two `NAME_REF`s, the dot). [`Machine::size_for`] reserves it past the stack's length, so
    /// a nested feed (fmt.rs's arguments) adds its own on top.
    pub(crate) vals: usize,
    /// The deepest nesting of open nodes, a float split's own `NAME_REF` counted.
    pub(crate) depth: usize,
}

impl Sizes {
    /// The sizes of `output`'s steps: one pass over its words, no allocation.
    pub(crate) fn of(output: &Output) -> Sizes {
        let (mut at, mut sizes, mut depth) = (0, Sizes::default(), 0usize);
        while let Some((step, next)) = output.step(at) {
            at = next;
            match step {
                OutlineStep::Step(Step::Token { .. }) | OutlineStep::Lazy { .. } => sizes.vals += 1,
                OutlineStep::Step(Step::Enter { .. }) => {
                    depth += 1;
                    sizes.vals += 1;
                    sizes.depth = sizes.depth.max(depth);
                }
                OutlineStep::Step(Step::Exit) => depth = depth.saturating_sub(1),
                // `apply`: a `NAME_REF` one deeper, and one exit of an open node per field.
                OutlineStep::Step(Step::FloatSplit { ends_in_dot }) => {
                    sizes.vals += 5;
                    sizes.depth = sizes.depth.max(depth + 1);
                    depth = depth.saturating_sub(if ends_in_dot { 1 } else { 2 });
                }
                OutlineStep::Step(Step::Error { .. }) => {}
            }
        }
        sizes
    }
}

fn run<'a>(m: &mut Machine<'_, '_, 'a>, output: &Output, toks: Toks<'_, '_, 'a>, mut expand: Option<&mut dyn Expand>) {
    let alloc = m.allocator();
    m.size_for(Sizes::of(output));
    let mut levels: AVec<'_, Level<'_, '_, '_, '_>> = Vec::new_in(alloc);
    levels.push(Level { parse: Parse::Base(output), at: 0, toks });
    // The calls this body has run, every nesting counted: past the budget, a call is refused.
    let mut spent = 0u32;
    while let Some(level) = levels.last_mut() {
        let Level { parse, at, toks } = level;
        let output = match parse {
            Parse::Base(output) => *output,
            Parse::Expansion(output) => &*output,
        };
        let Some((step, next)) = output.step(*at) else {
            levels.pop();
            continue;
        };
        *at = next;
        apply(m, step, toks);
        let Some(request) = m.take_request() else { continue };
        let Some(expand) = expand.as_deref_mut() else {
            m.unexpanded(request, Why::Kind);
            continue;
        };
        // The item's parse is level 1: each expansion inside it one deeper.
        if levels.len() > EXPANSION_DEPTH as usize {
            m.unexpanded(request, Why::TooDeep);
            continue;
        }
        if spent >= EXPANSION_BUDGET {
            m.unexpanded(request, Why::TooMany);
            continue;
        }
        spent += 1;
        let mut out = BodyOut::new_in(alloc);
        // The body's cfg set answers the call's `cfg!`; it is back before the next step.
        let cfg = m.lend_cfg();
        let mut locals: AVec<'_, LocalMacro<'_>> = Vec::new_in(alloc);
        locals.extend(m.local_macros());
        let call = BodyCall {
            path: request.path_range.of(m.text()),
            path_mark: m.mark_at(request.path_range.start),
            input: m.call_input(&request),
            input_marks: m.input_marks(&request),
            range: request.range,
            written: m.written_call(request.range),
            fragment: request.fragment,
            edition: m.edition,
            locals: &locals,
            expansions: m.expansions(),
        };
        let expansion = expand.expand_body(call, &mut *cfg, &mut out);
        // The locals read the machine's text, which the expansion is appended to next.
        drop(locals);
        m.return_cfg(cfg);
        match expansion {
            Expansion::Expanded => {
                let ranges = m.append_expansion(&request, &out);
                let tokens = parser::Tokens::new(&out.words.parser.words, true);
                let output = request.fragment.entry().parse(tokens);
                levels.push(Level { parse: Parse::Expansion(output), at: 0, toks: Toks::Words { ranges, pos: 0 } });
            }
            Expansion::Unsupported => m.unexpanded(request, Why::NotExpanded),
            Expansion::Failed => m.unexpanded(request, Why::ExpansionFailed),
        }
    }
}

/// One step to the machine.
fn apply(m: &mut Machine<'_, '_, '_>, step: OutlineStep<'_>, at: &mut Toks<'_, '_, '_>) {
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

/// Feeds `steps`, a parse's steps of `lexed`, to `m`.
pub(crate) fn feed_steps<'s>(
    m: &mut Machine<'_, '_, '_>,
    lexed: &LexedStr<'_>,
    steps: impl Iterator<Item = OutlineStep<'s>>,
    base: u32,
) {
    assert!(u32::try_from(lexed.as_str().len()).is_ok(), "an item of over 4 GiB: ranges are u32 pairs");
    let mut at = Toks::Lexed(Cursor { lexed, pos: 0, offset: 0, base });
    for step in steps {
        apply(m, step, &mut at);
        if let Some(request) = m.take_request() {
            m.unexpanded(request, Why::Kind);
        }
    }
}
