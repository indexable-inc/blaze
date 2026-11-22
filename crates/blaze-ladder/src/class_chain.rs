//! A refusal's class for the survey: its `Debug` form's leading chain of variant names
//! (`Check(Solve(Overflow`), never cut at its first parenthesis (walk-AH Brief 2: 420 refusals
//! of unrelated causes were one class, "callee: resolve Check").

/// The most characters of a string field [`class_chain`] keeps.
const CHAIN_TEXT_CHARS: usize = 80;

/// A refusal's typed name with its causes: the `Debug` form's leading chain of variant names,
/// `Outer(Inner(Leaf`, at most `depth` names (identifiers, each starting with a letter or `_`,
/// and `(` only, stopping at the first other character), then, where the chain ends at a
/// string field (`Unported("..")`, a fixed text), that text cut at [`CHAIN_TEXT_CHARS`]. No
/// field values (an `ItemKey`'s number, a node id): one cause stays one class.
pub(crate) fn class_chain(debug: &str, depth: usize) -> Box<str> {
    let bytes = debug.as_bytes();
    let mut end = 0;
    let mut names = 0;
    loop {
        let start = end;
        if end < bytes.len() && (bytes[end].is_ascii_alphabetic() || bytes[end] == b'_') {
            end += 1;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
        }
        if end == start {
            break;
        }
        names += 1;
        if names >= depth || bytes.get(end) != Some(&b'(') {
            break;
        }
        // The `(` opening the next level.
        end += 1;
    }
    let chain = &debug[..end];
    let text = match (chain.ends_with('('), bytes.get(end)) {
        (true, Some(b'"')) => {
            // The string field's text, up to its closing quote (an escaped `\"` is inside it).
            let rest = &debug[end + 1..];
            let mut escaped = false;
            let close = rest.char_indices().find(|&(_, c)| {
                let closes = c == '"' && !escaped;
                escaped = c == '\\' && !escaped;
                closes
            });
            let inner = &rest[..close.map_or(rest.len(), |(i, _)| i)];
            Some(inner.char_indices().nth(CHAIN_TEXT_CHARS).map_or(inner, |(i, _)| &inner[..i]))
        }
        _ => None,
    };
    match text {
        Some(text) => format!("{chain}\"{text}\"").into_boxed_str(),
        // A chain cut before a field that is no name (a number, a tuple) ends at its name.
        None => Box::from(chain.trim_end_matches('(')),
    }
}

#[cfg(test)]
mod tests {
    use super::class_chain;

    /// Expected: every assertion holds; the test prints nothing. A wrong fix (the old
    /// `class_of`, cut at the first `(`) gives `Check` for the first case and fails.
    #[test]
    fn class_chain_names_the_inner_variants_and_no_field_values() {
        // Three levels, the third's fields (an item key) dropped.
        assert_eq!(&*class_chain("Check(Solve(Overflow { depth: 7, item: ItemKey(1234) }))", 3), "Check(Solve(Overflow");
        // A deeper chain stops at `depth` names.
        assert_eq!(&*class_chain("Check(Malformed(Node(Edge(3))))", 3), "Check(Malformed(Node");
        // A fixed text (`Unported(&'static str)`) is kept: it names the form.
        assert_eq!(&*class_chain("Check(Unported(\"a `yield` \\\"here\\\"\"))", 3), "Check(Unported(\"a `yield` \\\"here\\\"\"");
        // A struct variant at the top is its name only: two keys never make two classes.
        assert_eq!(&*class_chain("NoImpl { trait_: ItemKey(1), item: ItemKey(2) }", 3), "NoImpl");
        assert_eq!(&*class_chain("NoImpl { trait_: ItemKey(9), item: ItemKey(8) }", 3), "NoImpl");
        // A number is no name: the chain ends at the name before it.
        assert_eq!(&*class_chain("Item(LibKey(42), Need(LibKey(7)))", 3), "Item(LibKey");
        assert_eq!(&*class_chain("Missing(42)", 3), "Missing");
    }
}
