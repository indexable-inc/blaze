//! Bytes the kernel hands back as text (a file name, a path a sandbox saw), with nothing
//! allocated past the `String` the caller keeps.

/// `bytes` added to `into` as [`String::from_utf8_lossy`] reads them, with nothing allocated.
pub fn push_lossy(into: &mut String, bytes: &[u8]) {
    for chunk in bytes.utf8_chunks() {
        into.push_str(chunk.valid());
        if !chunk.invalid().is_empty() {
            into.push(char::REPLACEMENT_CHARACTER);
        }
    }
}

#[cfg(test)]
mod tests {
    use hegel::TestCase;
    use hegel::generators as gs;

    use super::push_lossy;

    #[hegel::test]
    fn push_lossy_is_from_utf8_lossy(tc: TestCase) {
        let before = tc.draw(gs::text().max_size(8));
        let bytes: Vec<u8> = tc.draw(gs::binary().max_size(63));
        let text = tc.draw(gs::text().max_size(16));
        let mut out = before.clone();
        push_lossy(&mut out, &bytes);
        assert_eq!(&out[before.len()..], &*String::from_utf8_lossy(&bytes));
        let mut out = String::new();
        push_lossy(&mut out, text.as_bytes());
        assert_eq!(out, text);
    }

    #[test]
    fn each_bad_sequence_is_one_replacement() {
        let mut out = String::new();
        push_lossy(&mut out, b"a\xff\xfeb\xe2\x82c\xf0\x9f\x98\x80");
        assert_eq!(out, "a\u{fffd}\u{fffd}b\u{fffd}c\u{1f600}");
    }
}
