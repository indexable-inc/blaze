/// How many words the lines of `text` have.
pub fn count(text: &str) -> usize {
    text.lines().map(word::count).sum()
}
