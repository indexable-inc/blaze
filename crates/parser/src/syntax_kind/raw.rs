//! A `u16` as a [`SyntaxKind`], checked: the crate's one transmute outside the lexer's vector
//! loads (docs/v2/plan/design/unsafe.md). The safe version is a `match` of every kind or a
//! table of them, which `generated.rs` has neither of; a table costs a load per conversion
//! where this costs the compare it makes anyway.

use super::SyntaxKind;

impl From<u16> for SyntaxKind {
    #[inline]
    fn from(d: u16) -> SyntaxKind {
        assert!(d <= (SyntaxKind::__LAST as u16));
        // SAFETY: `SyntaxKind` is `#[repr(u16)]` with no explicit discriminants
        // (`generated.rs`), so its values are exactly `0..=__LAST`, which the assert above
        // checks `d` is in. A value past `__LAST` would be an invalid enum, undefined behavior.
        unsafe { std::mem::transmute::<u16, SyntaxKind>(d) }
    }
}

#[cfg(test)]
mod tests {
    use super::SyntaxKind;

    /// Every value up to `__LAST` is the kind of that discriminant, the safe reference.
    #[test]
    fn every_value_is_the_kind_of_its_discriminant() {
        for d in 0..=SyntaxKind::__LAST as u16 {
            assert_eq!(SyntaxKind::from(d) as u16, d);
        }
        assert_eq!(SyntaxKind::from(SyntaxKind::IDENT as u16), SyntaxKind::IDENT);
    }

    #[test]
    #[should_panic]
    fn a_value_past_the_last_kind_is_refused() {
        let _ = SyntaxKind::from(SyntaxKind::__LAST as u16 + 1);
    }
}
