//! arrow-buffer's `BitIndexIterator`: an `#[inline(always)]` `next` with a `?` on an `Option`.

pub type Chunks<'a> = std::iter::Chain<
    std::iter::Chain<std::option::IntoIter<u64>, std::iter::Copied<std::slice::Iter<'a, u64>>>,
    std::option::IntoIter<u64>,
>;

pub struct BitIndex<'a> {
    current: u64,
    offset: i64,
    iter: Chunks<'a>,
}

impl<'a> BitIndex<'a> {
    pub fn new(prefix: Option<u64>, words: &'a [u64], suffix: Option<u64>) -> Self {
        let mut iter = prefix.into_iter().chain(words.iter().copied()).chain(suffix);
        let current = iter.next().unwrap_or(0);
        BitIndex { current, offset: 0, iter }
    }
}

impl Iterator for BitIndex<'_> {
    type Item = usize;
    #[inline(always)]
    fn next(&mut self) -> Option<usize> {
        loop {
            if self.current != 0 {
                let bit = self.current.trailing_zeros();
                self.current &= self.current - 1;
                return Some((self.offset + bit as i64) as usize);
            }
            self.current = self.iter.next()?;
            self.offset += 64;
        }
    }
}
