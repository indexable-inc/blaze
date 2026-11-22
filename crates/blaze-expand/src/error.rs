//! The errors of an expansion that are not values of it: what stops the function itself. What a
//! macro call does wrong (no rule matched, `compile_error!`, an env var not set) is a
//! [`crate::Diagnostic`] in the expansion, a value the record keeps.

use core::fmt;

use crate::{
    call::CallShape,
    cx::HostError,
    def::{BuiltinAttr, BuiltinFn},
    expansion::TokenOrigin,
    tokens::Edition,
};

/// A table of a stream or an expansion whose `u32` index ran out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Table {
    Tokens,
    Text,
    Contexts,
    Notes,
    Reads,
}

/// Which stream of a call an error is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    /// The fn-like call's arguments, or the item of an attribute or a derive.
    Input,
    /// An attribute's arguments.
    AttrArgs,
    /// The definition's body.
    Def,
    /// A `macro m(..)` definition's parameters.
    Params,
    /// The answer of the eager call of this index.
    Answer(u32),
    /// What a proc macro gave back.
    Produced,
}

impl fmt::Display for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stream::Input => f.write_str("the call's input"),
            Stream::AttrArgs => f.write_str("the attribute's arguments"),
            Stream::Def => f.write_str("the definition's body"),
            Stream::Params => f.write_str("the definition's parameters"),
            Stream::Answer(at) => write!(f, "the answer to nested call {at}"),
            Stream::Produced => f.write_str("the proc macro's output"),
        }
    }
}

/// A token stream refused an operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamError {
    /// A table would pass `u32::MAX` entries or bytes.
    TooLarge { table: Table },
    /// A context names no context of its stream: a parent not yet pushed, or a token's context
    /// from another stream.
    NoSuchContext { ctx: u32 },
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::TooLarge { table } => {
                write!(f, "the {table:?} table of a token stream passed 2^32 entries")
            }
            StreamError::NoSuchContext { ctx } => {
                write!(f, "the syntax context {ctx} is none of its token stream's contexts")
            }
        }
    }
}

impl core::error::Error for StreamError {}

/// What this crate does not build yet: a typed stop, counted, never a guess.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unbuilt {
    BuiltinFn(BuiltinFn),
    BuiltinAttr(BuiltinAttr),
}

/// Why the expansion function stopped. A macro's own error is not one of these: it is a
/// [`crate::Diagnostic`] in the expansion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpandError {
    /// This kind of macro is not built here yet.
    NotBuilt { what: Unbuilt },
    /// A stream's table ran out, or a context was not its stream's.
    Stream { stream: Option<Stream>, error: StreamError },
    /// The context could not answer an outside read.
    Host(HostError),
    /// The call's kind is not one the macro can be called as (a derive called fn-like).
    WrongCallKind { def: CallShape, call: CallShape },
    /// An origin names a token its stream does not have.
    ForeignOrigin { origin: TokenOrigin },
    /// A stream's delimiters do not pair up: its token `token` closes nothing or is left open.
    Unbalanced { stream: Stream, token: u32 },
    /// A stream given as input holds a context marked [`crate::MarkOf::This`]: an expansion's
    /// tokens fed on without [`crate::TokenStream::seal`], whose marks would be read as this
    /// call's own.
    Unsealed { stream: Stream },
    /// An eager call was re-entered with a number of answers that is not its nested calls'.
    EagerAnswers { wanted: u32, given: u32 },
    /// A proc macro gave a number of origins that is not its number of tokens.
    ProcMacroOutput { tokens: u32, origins: u32 },
    /// A proc macro gave an origin (of a token or a diagnostic) naming a token it was not
    /// given: past its input or its attribute's arguments, or the definition's (`Def`,
    /// `Params`), which a proc macro has none of.
    ProcMacroOrigin { origin: TokenOrigin },
    /// A proc macro's diagnostic has a text that is not a whole `str` of the text it gave
    /// (`len` bytes long).
    ProcMacroText { text: crate::tokens::Text, len: u32 },
    /// A `macro_rules!` definition (`call: None`, while it was compiled) or a call of it went
    /// past the engine's bound on steps or nesting.
    RulesBound { bound: RulesLimit, def: crate::def::MacroContent, call: Option<crate::key::CallKey> },
}

/// The bounds of the `macro_rules!` engine: each makes a runaway definition or call a refusal
/// that names it, never a hang or an overflowed stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RulesLimit {
    /// Steps of matching and transcription in one call.
    Steps,
    /// Nested groups and repetitions, of the definition or the matcher's or transcriber's stack.
    Depth,
}

impl From<HostError> for ExpandError {
    fn from(error: HostError) -> ExpandError {
        ExpandError::Host(error)
    }
}

impl From<StreamError> for ExpandError {
    fn from(error: StreamError) -> ExpandError {
        ExpandError::Stream { stream: None, error }
    }
}

impl fmt::Display for ExpandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpandError::NotBuilt { what: Unbuilt::BuiltinFn(it) } => {
                write!(f, "the builtin macro `{}!` is not built in blaze-expand yet", it.name())
            }
            ExpandError::NotBuilt { what: Unbuilt::BuiltinAttr(it) } => {
                write!(
                    f,
                    "the builtin attribute `#[{}]` is not built in blaze-expand yet",
                    it.name()
                )
            }
            ExpandError::Stream { stream: Some(stream), error } => write!(f, "{stream}: {error}"),
            ExpandError::Stream { stream: None, error } => write!(f, "{error}"),
            ExpandError::Host(error) => write!(f, "{error}"),
            ExpandError::WrongCallKind { def, call } => {
                write!(f, "a {def:?} macro was called as a {call:?} macro")
            }
            ExpandError::ForeignOrigin { origin } => {
                write!(f, "the origin {origin:?} names no token of its stream")
            }
            ExpandError::Unbalanced { stream, token } => {
                write!(f, "{stream}: the delimiter at token {token} pairs with none")
            }
            ExpandError::Unsealed { stream } => write!(
                f,
                "{stream} holds a context of an expansion's own (`MarkOf::This`): seal an expansion's tokens with its key before they are input"
            ),
            ExpandError::EagerAnswers { wanted, given } => {
                write!(f, "an eager call has {wanted} nested calls and was given {given} answers")
            }
            ExpandError::ProcMacroOutput { tokens, origins } => write!(
                f,
                "a proc macro gave {tokens} tokens and {origins} origins: one origin per token"
            ),
            ExpandError::ProcMacroOrigin { origin } => {
                write!(f, "a proc macro gave the origin {origin:?}, which names no token it was given")
            }
            ExpandError::ProcMacroText { text, len } => write!(
                f,
                "a proc macro's diagnostic has the text {}..{} of {len} bytes, not a whole `str` of them",
                text.at,
                u64::from(text.at) + u64::from(text.len)
            ),
            ExpandError::RulesBound { bound, def, call } => {
                let what = match bound {
                    RulesLimit::Steps => "its bound on steps",
                    RulesLimit::Depth => "its bound on nesting",
                };
                match call {
                    Some(call) => write!(
                        f,
                        "the call {:?} of the macro {:?} went past {what} (rule 18)",
                        call.0, def.0
                    ),
                    None => write!(f, "the macro {:?} went past {what} while compiled (rule 18)", def.0),
                }
            }
        }
    }
}

impl core::error::Error for ExpandError {}

/// Text that does not lex.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LexError {
    /// The lexer found an error in the text (an unterminated literal, a stray character).
    Refused { edition: Edition },
    /// The text has more tokens or bytes than a stream holds.
    Stream(StreamError),
}

impl From<StreamError> for LexError {
    fn from(error: StreamError) -> LexError {
        LexError::Stream(error)
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LexError::Refused { edition } => {
                write!(f, "the text does not lex in edition {edition:?}")
            }
            LexError::Stream(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for LexError {}
