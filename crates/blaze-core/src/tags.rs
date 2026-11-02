//! The tags of one state machine. Several machines share a driver, so each is given its own
//! range `base..base + 2^32` and asks only with tags in it: a completion goes to the machine
//! that [`Tags::owns`] its tag.

use crate::Tag;

/// Tags `base..base + 2^32`, handed out in turn and wrapping within the range.
#[derive(Clone, Debug)]
pub struct Tags {
    base: u64,
    next: u64,
}

impl Tags {
    pub fn new(base: u64) -> Tags {
        Tags { base, next: 0 }
    }

    /// The next tag of the range.
    #[inline]
    pub fn next(&mut self) -> Tag {
        let tag = Tag(self.base.wrapping_add(self.next & 0xffff_ffff));
        self.next = self.next.wrapping_add(1);
        tag
    }

    /// Whether `tag` is in the range.
    #[inline]
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.base) < 1 << 32
    }
}

#[cfg(test)]
mod tests {
    use hegel::TestCase;
    use hegel::generators as gs;

    use super::*;

    /// blaze-cas's and blaze-global's copy as it was, the reference.
    struct Reference {
        base: u64,
        next: u64,
    }

    impl Reference {
        fn next(&mut self) -> Tag {
            let tag = Tag(self.base + (self.next & 0xffff_ffff));
            self.next = self.next.wrapping_add(1);
            tag
        }

        fn owns(&self, tag: Tag) -> bool {
            tag.0.wrapping_sub(self.base) < 1 << 32
        }
    }

    #[test]
    fn two_ranges_own_their_own_tags() {
        let mut a = Tags::new(1 << 32);
        let mut b = Tags::new(2 << 32);
        let (x, y) = (a.next(), b.next());
        assert!(a.owns(x) && !a.owns(y));
        assert!(b.owns(y) && !b.owns(x));
        assert_eq!(a.next(), Tag((1 << 32) + 1));
    }

    #[test]
    fn a_range_wraps_within_itself() {
        let mut tags = Tags { base: 5 << 32, next: 0xffff_ffff };
        assert_eq!(tags.next(), Tag((5 << 32) + 0xffff_ffff));
        assert_eq!(tags.next(), Tag(5 << 32));
    }

    #[hegel::test]
    fn hands_out_what_the_copies_did(tc: TestCase) {
        let base: u64 = tc.draw(gs::integers().max_value((u64::MAX >> 1) - 1));
        let start: u64 = tc.draw(gs::integers());
        let asks: usize = tc.draw(gs::integers().min_value(1).max_value(63));
        let probe: u64 = tc.draw(gs::integers());
        let mut tags = Tags { base, next: start };
        let mut reference = Reference { base, next: start };
        for _ in 0..asks {
            let tag = tags.next();
            assert_eq!(tag, reference.next());
            assert!(tags.owns(tag));
        }
        assert_eq!(tags.owns(Tag(probe)), reference.owns(Tag(probe)));
    }
}
