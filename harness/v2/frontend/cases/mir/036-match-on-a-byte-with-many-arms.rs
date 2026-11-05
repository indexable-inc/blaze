//@ expect: accept
fn class(b: u8) -> &'static str {
    match b {
        0 => "nul",
        b'\t' => "tab",
        b'\n' => "newline",
        b'\r' => "return",
        b' ' => "space",
        b'!' => "bang",
        b'"' => "quote",
        b'#' => "hash",
        b'$' => "dollar",
        b'%' => "percent",
        b'&' => "amp",
        b'\'' => "tick",
        b'(' => "open",
        b')' => "close",
        b'*' => "star",
        b'+' => "plus",
        b',' => "comma",
        b'-' => "minus",
        b'.' => "dot",
        b'/' => "slash",
        b'0' => "zero",
        b'1'..=b'9' => "digit",
        b':' => "colon",
        b';' => "semi",
        b'<' => "less",
        b'=' => "equals",
        b'>' => "greater",
        b'?' => "question",
        b'@' => "at",
        b'A'..=b'F' => "hex upper",
        b'G'..=b'Z' => "upper",
        b'[' => "bracket",
        b'\\' => "backslash",
        b']' => "unbracket",
        b'^' => "caret",
        b'_' => "underscore",
        b'`' => "backtick",
        b'a'..=b'f' => "hex lower",
        b'x' => "ex",
        b'g'..=b'z' => "lower",
        b'{' => "brace",
        b'|' => "pipe",
        b'}' => "unbrace",
        b'~' => "tilde",
        128.. => "high",
        _ => "control",
    }
}

fn main() {
    for b in [0u8, 9, 10, 32, 34, 48, 55, 65, 70, 71, 90, 97, 102, 103, 120, 122, 126, 127, 200, 255, 1] {
        println!("{b} {}", class(b));
    }
}
