//! A bit-set of `SyntaxKind`s.

use blaze_ids::bits::BitArray;

use crate::SyntaxKind;

/// A bit-set of `SyntaxKind`s
#[derive(Clone, Copy)]
pub(crate) struct TokenSet(BitArray<3>);

/// `TokenSet`s should only include token `SyntaxKind`s, so the discriminant of any passed/included
/// `SyntaxKind` must *not* be greater than that of the last token `SyntaxKind`.
/// See #17037.
const LAST_TOKEN_KIND_DISCRIMINANT: usize = SyntaxKind::SHEBANG as usize;

impl TokenSet {
    pub(crate) const EMPTY: TokenSet = TokenSet(BitArray::EMPTY);

    pub(crate) const fn new(kinds: &[SyntaxKind]) -> TokenSet {
        let mut res: BitArray<3> = BitArray::EMPTY;
        let mut i = 0;
        while i < kinds.len() {
            let discriminant = kinds[i] as usize;
            debug_assert!(
                discriminant <= LAST_TOKEN_KIND_DISCRIMINANT,
                "Expected a token `SyntaxKind`"
            );
            res = res.with_bit(discriminant);
            i += 1;
        }
        TokenSet(res)
    }

    pub(crate) const fn union(self, other: TokenSet) -> TokenSet {
        TokenSet(self.0.or(&other.0))
    }

    pub(crate) const fn contains(&self, kind: SyntaxKind) -> bool {
        let discriminant = kind as usize;
        debug_assert!(
            discriminant <= LAST_TOKEN_KIND_DISCRIMINANT,
            "Expected a token `SyntaxKind`"
        );
        self.0.has_bit(discriminant)
    }
}

#[test]
fn token_set_works_for_tokens() {
    use crate::SyntaxKind::*;
    let ts = TokenSet::new(&[EOF, SHEBANG]);
    assert!(ts.contains(EOF));
    assert!(ts.contains(SHEBANG));
    assert!(!ts.contains(PLUS));
}
