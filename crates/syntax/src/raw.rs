//! The crate's one unsafe module (docs/v2/plan/design/unsafe.md): a text built straight into
//! its shared allocation. The safe way, a `String` and then `Arc::<str>::from`, copies the
//! whole file twice per edit and allocates twice; this copies it once.

use std::sync::Arc;

use syntax_tree::{Text, TextRange};

/// `parts` one after the other, written once into the shared text's allocation.
pub(crate) fn joined(parts: &[&str]) -> Text {
    joined_from(parts.iter().copied())
}

/// `source` with each of `edits` made, written once into the shared text's allocation: each
/// range of `source` replaced by its text. They are in order and none overlaps another.
pub(crate) fn spliced(source: &str, edits: &[(TextRange, &str)]) -> Text {
    let mut end = 0;
    for &(range, _) in edits {
        let (from, to) = (usize::from(range.start()), usize::from(range.end()));
        assert!(
            end <= from && to <= source.len(),
            "the edit of {range:?} is before the one ending at {end}, or past the text's {} bytes",
            source.len(),
        );
        end = to;
    }
    let ends = std::iter::once(0).chain(edits.iter().map(|(range, _)| usize::from(range.end())));
    let parts = ends.zip(edits.iter().map(Some).chain([None])).flat_map(|(from, edit)| {
        let to = edit.map_or(source.len(), |(range, _)| usize::from(range.start()));
        [&source[from..to], edit.map_or("", |&(_, text)| text)]
    });
    joined_from(parts)
}

/// [`joined`] of `parts`, which are walked twice: for the length, then to copy.
fn joined_from<'a>(parts: impl Iterator<Item = &'a str> + Clone) -> Text {
    // clone: an iterator of borrowed parts, walked once for the length
    let len = parts.clone().map(|it| it.len()).sum();
    let mut bytes = Arc::<[u8]>::new_uninit_slice(len);
    let out = Arc::get_mut(&mut bytes).expect("a new Arc has no other owner");
    let mut at = 0;
    for part in parts {
        out[at..at + part.len()].write_copy_of_slice(part.as_bytes());
        at += part.len();
    }
    assert_eq!(at, len, "every byte of the text is written");
    // SAFETY: the loop wrote `out[0..len]`, all of it (the assert above); reading an
    // unwritten byte would be undefined behavior.
    let bytes = unsafe { bytes.assume_init() };
    // SAFETY: the bytes are whole `&str`s one after the other, so valid UTF-8, and `str` has
    // the layout of `[u8]`, so the pointer from `into_raw` is a valid `Arc<str>` allocation.
    // Bytes that were not UTF-8 would break every `str` method on the result.
    let text: Arc<str> = unsafe { Arc::from_raw(Arc::into_raw(bytes) as *const str) };
    Text::from(text)
}

#[cfg(test)]
mod tests {
    /// The safe reference version (`concat`) gives the same text.
    #[test]
    fn joined_is_the_concatenation() {
        let long = "x".repeat(100_000);
        let cases: [&[&str]; 6] = [
            &[],
            &[""],
            &["a"],
            &["", "é", ""],
            &["fn f() {", " x ", "}\n", "名"],
            &[long.as_str(), "🦀", "y"],
        ];
        for parts in cases {
            assert_eq!(&*super::joined(parts), parts.concat());
        }
    }

    /// The safe reference version (a `String` the edits are made in from the last) gives the
    /// same text.
    #[test]
    fn spliced_is_the_text_with_the_edits_made() {
        use syntax_tree::{TextRange, TextSize};
        let range = |from: u32, to: u32| TextRange::new(TextSize::new(from), TextSize::new(to));
        let source = "fn f() { let x = 1; }";
        let cases: [&[(TextRange, &str)]; 5] = [
            &[],
            &[(range(0, 0), "pub ")],
            &[(range(3, 4), "g"), (range(13, 14), "名"), (range(21, 21), "\n")],
            &[(range(0, 21), "")],
            &[(range(9, 9), "a"), (range(9, 12), "b"), (range(12, 12), "c")],
        ];
        for edits in cases {
            let mut expected = source.to_owned();
            for &(range, text) in edits.iter().rev() {
                expected.replace_range(usize::from(range.start())..usize::from(range.end()), text);
            }
            assert_eq!(&*super::spliced(source, edits), expected);
        }
    }
}
