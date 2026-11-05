/// How many words `text` has.
///
/// ```
/// assert_eq!(word::count("one two"), 2);
/// ```
#[cfg(feature = "split")]
pub fn count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// The longest word of `text`.
///
/// ```
/// assert_eq!(word::longest("a bcd ef"), Some("bcd"));
/// ```
///
/// An example has the overflow checks of a test, in the standard library's `#[inline]`
/// arithmetic too:
///
/// ```should_panic
/// let length: &u32 = &std::hint::black_box(2);
/// let _ = length - 3;
/// ```
pub fn longest(text: &str) -> Option<&str> {
    text.split_whitespace().max_by_key(|it| it.len())
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts() {
        assert_eq!(super::count(" a  b c "), 3);
    }
}
