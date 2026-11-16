//! One type from its text alone: the parser's own type entry ([`parser::TopEntryPoint::Type`])
//! fed to the lowering's machine in [`Mode::Type`], so a type read from text (an unexpanded
//! type macro's input, `core::pattern_type!(u32 is 1..)`'s `u32`) lowers to the same
//! [`TypeRef`] tree, node for node, a signature's type of that text does.

use core::fmt;

use blaze_own::Alloc;
use parser::{LexedStr, Step, SyntaxKind};

use crate::machine::{Machine, Mode};
use crate::driver::Expand;
use crate::{Cfg, Edition, Lowered, Need, feed, parser_edition};

/// Why a text is not lowered as one type. Offsets are bytes into the text; `token` is the
/// text of the token there (empty at the text's end).
#[derive(Debug)]
pub enum TypeTextRefused<'a, 't> {
    /// No token at all (nothing, or whitespace and comments).
    Empty,
    /// A type and then more: `token`, at `offset`, is the first token after it.
    Trailing { offset: u32, token: &'t str },
    /// The lexer or the parser met an error at `offset`: no type is made from a text that is
    /// not one.
    Syntax { offset: u32, token: &'t str },
    /// The lowering met `cfg` atoms not known yet (a fn pointer's `#[cfg]` parameter).
    Need(Need<'a>),
}

impl fmt::Display for TypeTextRefused<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeTextRefused::Empty => f.write_str("lower_type_text: the text holds no type (no token)"),
            TypeTextRefused::Trailing { offset, token } => {
                write!(f, "lower_type_text: tokens after the type, from `{token}` at byte {offset}")
            }
            TypeTextRefused::Syntax { offset, token } => {
                write!(f, "lower_type_text: not a type: a syntax error at `{token}`, byte {offset}")
            }
            TypeTextRefused::Need(need) => {
                write!(f, "lower_type_text: {} cfg atoms not known yet", need.atoms.len())
            }
        }
    }
}

/// Lowers `text`, one type and nothing else, to a record whose [`crate::View::type_root`] is
/// its root: a [`TypeRef`] tree with paths as path rows and no body scope, as a signature's
/// type of the same text. A macro call in it is kept as the unexpanded type-macro row a
/// signature's is (nothing is expanded here).
///
/// # Panics
///
/// When `text` is longer than `u32::MAX` bytes.
///
/// [`TypeRef`]: crate::TypeRef
pub fn lower_type_text<'a, 't>(
    text: &'t str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, TypeTextRefused<'a, 't>> {
    lower_type(text, edition, cfg, None, alloc)
}

/// As [`lower_type_text`], with each macro call in the type expanded through `expand` and its
/// expansion lowered in place (core's `pattern_type!(T is PAT)` becomes a
/// [`crate::TypeRef::Pat`]). A signature is lowered unexpanded; its reader expands a type
/// macro's own text through this.
pub fn lower_type_text_expanding<'a, 't>(
    text: &'t str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    expand: &mut dyn Expand,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, TypeTextRefused<'a, 't>> {
    expand.begin_body();
    lower_type(text, edition, cfg, Some(expand), alloc)
}

fn lower_type<'a, 't>(
    text: &'t str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    expand: Option<&mut dyn Expand>,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, TypeTextRefused<'a, 't>> {
    assert!(u32::try_from(text.len()).is_ok(), "a type text of over 4 GiB: ranges are u32 pairs");
    let pe = parser_edition(edition);
    let lexed = LexedStr::new(pe, text);
    if let Some((i, _)) = lexed.errors().next() {
        return Err(syntax_at_lexed(text, &lexed, i));
    }
    if (0..lexed.len()).all(|i| lexed.kind(i).is_trivia()) {
        return Err(TypeTextRefused::Empty);
    }
    let input = lexed.to_input(pe);
    let output = parser::TopEntryPoint::Type.parse(input.tokens());
    // The entry wraps a type with tokens after it in one `ERROR` node; a token directly in
    // that node is the first after the type. An error step names the token it stands before.
    let mut depth = 0usize;
    let mut wrapped = false;
    let mut consumed = 0usize;
    for (n, step) in output.iter().enumerate() {
        match step {
            Step::Enter { kind } => {
                if n == 0 && kind == SyntaxKind::ERROR {
                    wrapped = true;
                }
                depth += 1;
            }
            Step::Exit => depth -= 1,
            Step::Token { n_input_tokens, .. } => {
                if wrapped && depth == 1 {
                    let (offset, token) = input_token_at(text, &lexed, consumed);
                    return Err(TypeTextRefused::Trailing { offset, token });
                }
                consumed += usize::from(n_input_tokens);
            }
            Step::FloatSplit { .. } => {}
            Step::Error { .. } => {
                let (offset, token) = input_token_at(text, &lexed, consumed);
                return Err(TypeTextRefused::Syntax { offset, token });
            }
        }
    }
    let mut m = Machine::new(text, Mode::Type, edition, cfg, None, alloc);
    match expand {
        Some(expand) => feed::feed_expanding(&mut m, &lexed, &output, 0, expand),
        None => feed::feed(&mut m, &lexed, &output, 0),
    }
    m.finish().map_err(TypeTextRefused::Need)
}

/// The offset and text of the `n`th input token (the `n`th token that is not trivia), or the
/// text's end and `""` past the last.
fn input_token_at<'t>(text: &'t str, lexed: &LexedStr<'_>, n: usize) -> (u32, &'t str) {
    let mut seen = 0usize;
    for i in 0..lexed.len() {
        if lexed.kind(i).is_trivia() {
            continue;
        }
        if seen == n {
            let r = lexed.text_range(i);
            return (offset(r.start), &text[r]);
        }
        seen += 1;
    }
    (offset(text.len()), "")
}

fn syntax_at_lexed<'a, 't>(text: &'t str, lexed: &LexedStr<'_>, i: usize) -> TypeTextRefused<'a, 't> {
    let r = lexed.text_range(i);
    TypeTextRefused::Syntax { offset: offset(r.start), token: &text[r] }
}

fn offset(at: usize) -> u32 {
    u32::try_from(at).expect("checked: the text is under 4 GiB")
}
