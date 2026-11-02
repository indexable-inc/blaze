//! See [`Output`]

use std::borrow::Cow;

use crate::SyntaxKind;

/// Output of the parser -- a DFS traversal of a concrete syntax tree.
///
/// Use the [`Output::iter`] method to iterate over traversal steps and consume
/// a syntax tree.
///
/// In a sense, this is just a sequence of [`SyntaxKind`]-colored parenthesis
/// interspersed into the original [`crate::Input`]. The output is fundamentally
/// coordinated with the input and `n_input_tokens` refers to the number of
/// times [`crate::Input::push`] was called.
pub struct Output {
    /// 32-bit encoding of events. If LSB is zero, then that's an index into the
    /// error vector. Otherwise, it's one of the three other variants, with data encoded as
    ///
    /// ```text
    /// |16 bit kind|8 bit n_input_tokens|4 bit tag|4 bit leftover|
    /// ```
    ///
    /// A lazy node's word is followed by one more, its number of input tokens.
    event: blaze_mem::scratch::Lent<Words>,
    /// The syntax errors: most are fixed text, borrowed, and only a formatted one is owned.
    error: Vec<Cow<'static, str>>,
}

crate::event::per_thread_buffer! {
    /// An output's words: kept per thread at the most they ever held, back on the shelf of
    /// the thread that drops the output.
    Words, WORDS: u32
}

impl Default for Output {
    fn default() -> Output {
        Output::with_event_capacity(0)
    }
}

#[derive(Debug)]
pub enum Step<'a> {
    Token { kind: SyntaxKind, n_input_tokens: u8 },
    FloatSplit { ends_in_dot: bool },
    Enter { kind: SyntaxKind },
    Exit,
    Error { msg: &'a str },
}

/// A step of an outline's output ([`crate::TopEntryPoint::parse_outline`]): one of a full
/// parse, or a node the parser skipped.
#[derive(Debug)]
pub enum OutlineStep<'a> {
    Step(Step<'a>),
    /// A node of `kind` over the next `n_input_tokens` input tokens, not parsed.
    Lazy { kind: SyntaxKind, n_input_tokens: u32 },
}

impl Output {
    /// Preallocate the event buffer. Each `Event` in the input stream maps to
    /// roughly one `u32` in the output, so passing the event count as the
    /// initial capacity avoids most of the amortized `Vec::grow` allocations
    /// during `event::process`.
    pub(crate) fn with_event_capacity(cap: usize) -> Self {
        let mut event = blaze_mem::scratch::lend::<Words>();
        event.reserve(cap);
        Output { event, error: Vec::new() } // alloc: empty until the first syntax error
    }

    const EVENT_MASK: u32 = 0b1;
    const TAG_MASK: u32 = 0x0000_00F0;
    const N_INPUT_TOKEN_MASK: u32 = 0x0000_FF00;
    const KIND_MASK: u32 = 0xFFFF_0000;

    const ERROR_SHIFT: u32 = Self::EVENT_MASK.trailing_ones();
    const TAG_SHIFT: u32 = Self::TAG_MASK.trailing_zeros();
    const N_INPUT_TOKEN_SHIFT: u32 = Self::N_INPUT_TOKEN_MASK.trailing_zeros();
    const KIND_SHIFT: u32 = Self::KIND_MASK.trailing_zeros();

    const TOKEN_EVENT: u8 = 0;
    const ENTER_EVENT: u8 = 1;
    const EXIT_EVENT: u8 = 2;
    const SPLIT_EVENT: u8 = 3;
    /// Followed by a word that is the number of input tokens.
    const LAZY_EVENT: u8 = 4;

    /// The steps of a full parse. Panics at a lazy node: read an outline with [`Output::steps`].
    pub fn iter(&self) -> impl Iterator<Item = Step<'_>> {
        self.steps().map(|step| match step {
            OutlineStep::Step(step) => step,
            OutlineStep::Lazy { kind, .. } => {
                panic!("a lazy {kind:?} in a full parse's output: read an outline with `steps`")
            }
        })
    }

    /// The steps of a full parse or of an outline.
    pub fn steps(&self) -> impl Iterator<Item = OutlineStep<'_>> {
        let mut at = 0;
        std::iter::from_fn(move || {
            let event = *self.event.get(at)?;
            at += 1;
            if event & Self::EVENT_MASK == 0 {
                return Some(OutlineStep::Step(Step::Error {
                    msg: &self.error[(event as usize) >> Self::ERROR_SHIFT],
                }));
            }
            let tag = ((event & Self::TAG_MASK) >> Self::TAG_SHIFT) as u8;
            let kind =
                || -> SyntaxKind { (((event & Self::KIND_MASK) >> Self::KIND_SHIFT) as u16).into() };
            let step = match tag {
                Self::TOKEN_EVENT => {
                    let n_input_tokens =
                        ((event & Self::N_INPUT_TOKEN_MASK) >> Self::N_INPUT_TOKEN_SHIFT) as u8;
                    Step::Token { kind: kind(), n_input_tokens }
                }
                Self::ENTER_EVENT => Step::Enter { kind: kind() },
                Self::EXIT_EVENT => Step::Exit,
                Self::SPLIT_EVENT => {
                    Step::FloatSplit { ends_in_dot: event & Self::N_INPUT_TOKEN_MASK != 0 }
                }
                Self::LAZY_EVENT => {
                    let n_input_tokens = self.event[at];
                    at += 1;
                    return Some(OutlineStep::Lazy { kind: kind(), n_input_tokens });
                }
                _ => unreachable!(),
            };
            Some(OutlineStep::Step(step))
        })
    }

    /// The steps of the first node alone, its kind, and the number of input tokens it took:
    /// for a parse that goes on past the node it is after
    /// ([`crate::Reparser::top_level_item`]). `None` when the steps do not start with a node.
    pub fn into_first_node(mut self) -> Option<(Output, SyntaxKind, usize)> {
        let mut root = None;
        let mut depth = 0usize;
        let mut tokens = 0usize;
        let mut at = 0;
        while at < self.event.len() {
            let event = self.event[at];
            at += 1;
            if event & Self::EVENT_MASK == 0 {
                if depth == 0 {
                    return None;
                }
                continue;
            }
            match ((event & Self::TAG_MASK) >> Self::TAG_SHIFT) as u8 {
                Self::TOKEN_EVENT => {
                    if depth == 0 {
                        return None;
                    }
                    tokens += ((event & Self::N_INPUT_TOKEN_MASK) >> Self::N_INPUT_TOKEN_SHIFT)
                        as usize;
                }
                Self::ENTER_EVENT => {
                    if depth == 0 {
                        let kind: SyntaxKind =
                            (((event & Self::KIND_MASK) >> Self::KIND_SHIFT) as u16).into();
                        root = Some(kind);
                    }
                    depth += 1;
                }
                Self::EXIT_EVENT => depth = depth.checked_sub(1)?,
                Self::SPLIT_EVENT => {
                    let ends_in_dot = event & Self::N_INPUT_TOKEN_MASK != 0;
                    tokens += 1;
                    depth = depth.checked_sub(1 + usize::from(!ends_in_dot))?;
                }
                Self::LAZY_EVENT => {
                    if depth == 0 {
                        return None;
                    }
                    tokens += self.event[at] as usize;
                    at += 1;
                }
                _ => unreachable!(),
            }
            if depth == 0 {
                self.event.truncate(at);
                return Some((self, root?, tokens));
            }
        }
        None
    }

    pub(crate) fn token(&mut self, kind: SyntaxKind, n_tokens: u8) {
        let e = ((kind as u16 as u32) << Self::KIND_SHIFT)
            | ((n_tokens as u32) << Self::N_INPUT_TOKEN_SHIFT)
            | Self::EVENT_MASK;
        self.event.push(e)
    }

    pub(crate) fn float_split_hack(&mut self, ends_in_dot: bool) {
        let e = ((Self::SPLIT_EVENT as u32) << Self::TAG_SHIFT)
            | ((ends_in_dot as u32) << Self::N_INPUT_TOKEN_SHIFT)
            | Self::EVENT_MASK;
        self.event.push(e);
    }

    pub(crate) fn enter_node(&mut self, kind: SyntaxKind) {
        let e = ((kind as u16 as u32) << Self::KIND_SHIFT)
            | ((Self::ENTER_EVENT as u32) << Self::TAG_SHIFT)
            | Self::EVENT_MASK;
        self.event.push(e)
    }

    pub(crate) fn leave_node(&mut self) {
        let e = ((Self::EXIT_EVENT as u32) << Self::TAG_SHIFT) | Self::EVENT_MASK;
        self.event.push(e)
    }

    pub(crate) fn lazy(&mut self, kind: SyntaxKind, n_input_tokens: u32) {
        let e = ((kind as u16 as u32) << Self::KIND_SHIFT)
            | ((Self::LAZY_EVENT as u32) << Self::TAG_SHIFT)
            | Self::EVENT_MASK;
        self.event.push(e);
        self.event.push(n_input_tokens);
    }

    pub(crate) fn error(&mut self, error: Cow<'static, str>) {
        let idx = self.error.len();
        self.error.push(error);
        let e = (idx as u32) << Self::ERROR_SHIFT;
        self.event.push(e);
    }
}
