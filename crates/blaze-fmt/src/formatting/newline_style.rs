// Follows `src/tools/rustfmt/src/formatting/newline_style.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use crate::NewlineStyle;

/// Apply this newline style to the formatted text. When the style is set
/// to `Auto`, the `raw_input_text` is used to detect the existing line
/// endings.
///
/// If the style is set to `Auto` and `raw_input_text` contains no
/// newlines, the `Native` style will be used.
pub(crate) fn apply_newline_style(
    newline_style: NewlineStyle,
    formatted_text: &mut String,
    raw_input_text: &str,
) {
    *formatted_text = match effective_newline_style(newline_style, raw_input_text) {
        EffectiveNewlineStyle::Windows => convert_to_windows_newlines(formatted_text),
        EffectiveNewlineStyle::Unix => convert_to_unix_newlines(formatted_text),
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum EffectiveNewlineStyle {
    Windows,
    Unix,
}

fn effective_newline_style(
    newline_style: NewlineStyle,
    raw_input_text: &str,
) -> EffectiveNewlineStyle {
    match newline_style {
        NewlineStyle::Auto => auto_detect_newline_style(raw_input_text),
        NewlineStyle::Native => native_newline_style(),
        NewlineStyle::Windows => EffectiveNewlineStyle::Windows,
        NewlineStyle::Unix => EffectiveNewlineStyle::Unix,
    }
}

const LINE_FEED: char = '\n';
const CARRIAGE_RETURN: char = '\r';
const WINDOWS_NEWLINE: &str = "\r\n";
const UNIX_NEWLINE: &str = "\n";

fn auto_detect_newline_style(raw_input_text: &str) -> EffectiveNewlineStyle {
    let first_line_feed_pos = raw_input_text.chars().position(|ch| ch == LINE_FEED);
    match first_line_feed_pos {
        Some(first_line_feed_pos) => {
            let char_before_line_feed_pos = first_line_feed_pos.saturating_sub(1);
            let char_before_line_feed = raw_input_text.chars().nth(char_before_line_feed_pos);
            match char_before_line_feed {
                Some(CARRIAGE_RETURN) => EffectiveNewlineStyle::Windows,
                _ => EffectiveNewlineStyle::Unix,
            }
        }
        None => native_newline_style(),
    }
}

fn native_newline_style() -> EffectiveNewlineStyle {
    if cfg!(windows) {
        EffectiveNewlineStyle::Windows
    } else {
        EffectiveNewlineStyle::Unix
    }
}

fn convert_to_windows_newlines(formatted_text: &String) -> String {
    let mut transformed = String::with_capacity(2 * formatted_text.capacity());
    let mut chars = formatted_text.chars().peekable();
    while let Some(current_char) = chars.next() {
        let next_char = chars.peek();
        match current_char {
            LINE_FEED => transformed.push_str(WINDOWS_NEWLINE),
            CARRIAGE_RETURN if next_char == Some(&LINE_FEED) => {}
            current_char => transformed.push(current_char),
        }
    }
    transformed
}

fn convert_to_unix_newlines(formatted_text: &str) -> String {
    formatted_text.replace(WINDOWS_NEWLINE, UNIX_NEWLINE)
}
