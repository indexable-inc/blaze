use std::borrow::Cow;

use rustc_literal_escaper::{EscapeError, MixedUnit, unescape_c_str};

/// The value of a C string literal with `text` between its quotes.
/// The text has the line endings of a source file the compiler has read, which are `\n` only.
pub(crate) fn unescaped_c_str(text: &str, raw: bool) -> Result<Cow<'_, [u8]>, EscapeError> {
    if raw {
        return Ok(Cow::Borrowed(text.as_bytes()));
    }

    // alloc: grown only once an escape makes the value differ from the text.
    let mut buf = Vec::new();
    let mut prev_end = 0;
    let mut has_error = None;
    let extend_unit = |buf: &mut Vec<u8>, unit: MixedUnit| match unit {
        MixedUnit::Char(c) => buf.extend(c.get().encode_utf8(&mut [0; 4]).as_bytes()),
        MixedUnit::HighByte(b) => buf.push(b.get()),
    };
    unescape_c_str(text, |char_range, unescaped| match (unescaped, buf.capacity() == 0) {
        (Ok(u), false) => extend_unit(&mut buf, u),
        (Ok(_), true) if char_range.len() == 1 && char_range.start == prev_end => {
            prev_end = char_range.end
        }
        (Ok(u), true) => {
            buf.reserve_exact(text.len());
            buf.extend(&text.as_bytes()[..prev_end]);
            extend_unit(&mut buf, u);
        }
        (Err(e), _) if e.is_fatal() => has_error = Some(e),
        // A warning about a line continuation, which gives no character.
        (Err(_), _) => {}
    });

    match (has_error, buf.capacity() == 0) {
        (Some(e), _) => Err(e),
        (None, true) => Ok(Cow::Borrowed(&text.as_bytes()[..prev_end])),
        (None, false) => Ok(Cow::Owned(buf)),
    }
}
