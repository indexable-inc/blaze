//! A text written again from its own tokens, as a renderer of token trees writes one: the
//! proof's input for `outline_tokens` (tests/rows.rs on fixed texts, examples/oracle.rs over
//! the corpora). The writer here is not the lexer's inverse by the source's bytes: every
//! stretch between two tokens (whitespace, comments that are no doc comments, whatever their
//! length) is written as one line feed, a stretch of none stays none, and each token's bytes
//! are copied. So the text written differs from the source wherever the source had a comment
//! or more than a line feed, and the tokens handed over are exactly those the lexer makes of
//! the text written ([`lexes_back`] checks that, so a proof never rests on it).
//!
//! A line feed and not a space: a line doc comment (`/// ..`, a token) ends at its line's end.

use blaze_file_items::{TokenColumns, TokensRefused};
use blaze_surface::Edition;
use parser::{JOINT, Joiner, LexedStr};

/// The parser's edition of an outline's.
pub fn parser_edition(edition: Edition) -> parser::Edition {
    match edition {
        Edition::E2015 => parser::Edition::Edition2015,
        Edition::E2018 => parser::Edition::Edition2018,
        Edition::E2021 => parser::Edition::Edition2021,
        Edition::E2024 => parser::Edition::Edition2024,
    }
}

/// Writes the tokens of `text` again: the text into `out`, each token into `columns` as it is
/// written (one pass, as a renderer does it), and into `written` its word without the joint
/// mark, its start and its end, for [`lexes_back`]. The words given to `columns` are the
/// lexer's as they are, a float's own joint mark included: `TokenColumns::push` reads only
/// what a word says of its token alone.
pub fn rerender(
    text: &str,
    edition: Edition,
    out: &mut String,
    columns: &mut TokenColumns<'_>,
    written: &mut Vec<(u32, u32, u32)>,
) -> Result<(), TokensRefused> {
    let edition = parser_edition(edition);
    out.clear();
    columns.clear();
    written.clear();
    let lexed = LexedStr::new(edition, text);
    let mut end = 0;
    let mut pushed = Ok(());
    lexed.each_input_token(edition, &mut Joiner::new(), |word, bytes, _| {
        if bytes.start != end {
            out.push('\n');
        }
        end = bytes.end;
        let start = u32::try_from(out.len()).expect("a text an outline takes");
        out.push_str(&text[bytes]);
        let stop = u32::try_from(out.len()).expect("a text an outline takes");
        written.push((word & !JOINT, start, stop));
        if pushed.is_ok() {
            pushed = columns.push(word, start, stop);
        }
    });
    if end != text.len() {
        out.push('\n');
    }
    pushed
}

/// Whether the lexer makes of `text` exactly the tokens `written` ([`rerender`]'s): the same
/// words, joint marks aside, at the same bytes.
pub fn lexes_back(text: &str, edition: Edition, written: &[(u32, u32, u32)]) -> bool {
    let edition = parser_edition(edition);
    let lexed = LexedStr::new(edition, text);
    let (mut at, mut same) = (0, true);
    lexed.each_input_token(edition, &mut Joiner::new(), |word, bytes, _| {
        let token = (word & !JOINT, bytes.start as u32, bytes.end as u32);
        same &= written.get(at) == Some(&token);
        at += 1;
    });
    same && at == written.len()
}
