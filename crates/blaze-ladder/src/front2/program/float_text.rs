//! What a float literal's text says, for the seam's `float_lit` and `float_pat`.
//!
//! The Reference, "Literal expressions", "Floating-point literal expressions": "The value of
//! the expression is determined from the string representation of the token as follows: Any
//! suffix is removed from the string. Any underscores are removed from the string. The rest of
//! the string is converted to the expression's type as if by `f32::from_str` or
//! `f64::from_str`." So the text is rounded ONCE to each type from its decimal digits (never
//! to `f64` and then narrowed: that rounds twice and can move an `f32`'s last bit), by core's
//! correctly rounded parse ([`super::float_bits`]: the tree's one decimal-to-float routine for
//! this seam).
//!
//! The seam carries a value for `f32` and `f64` only (`FrontEnd::float_lit`: an `f64`'s bits
//! and an `f32`'s). A literal suffixed `f16` or `f128` has no value in that form, and the
//! builder has no form for those types (blaze-build walk/numbers.rs `float_repr` refuses
//! "`f16` or `f128`" by name, before it asks any literal's value): such a literal is
//! [`FloatText::NoSeamForm`], filed with no value, so the fn holding it is tabled and each
//! instance of it is refused by the builder by that name, never the whole program by front2.

use super::float_bits;

/// What a float literal's text (a pattern's: its magnitude) says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FloatText {
    /// Its value rounded to the nearest `f64` and to the nearest `f32`, each once from the
    /// text: the checker's type for the literal picks one (run.rs `FrontAdts::float_lit`).
    Bits { wide: u64, narrow: u32 },
    /// The literal is suffixed with a float type (named) the seam carries no value for.
    NoSeamForm(&'static str),
    /// No float literal: core's parse refuses the text left when the suffix and the
    /// underscores are dropped.
    Unread,
}

/// The suffixes the seam has no value form for. `f128` is asked first: no text ends in both,
/// and neither `f32` nor `f64` is a tail of either.
const NO_SEAM_FORM: [&str; 2] = ["f128", "f16"];

/// `text` (`1.5`, `2e-3_f32`, `1_000.0`, `0.0f16`) as [`FloatText`].
pub(super) fn float_text(text: &str) -> FloatText {
    let text = text.trim();
    for suffix in NO_SEAM_FORM {
        // A suffix alone is no literal (`f16` is a name): digits must come before it.
        if text.strip_suffix(suffix).is_some_and(|digits| digits.starts_with(|c: char| c.is_ascii_digit())) {
            return FloatText::NoSeamForm(suffix);
        }
    }
    match float_bits(text) {
        Some((wide, narrow)) => FloatText::Bits { wide, narrow },
        None => FloatText::Unread,
    }
}

#[cfg(test)]
mod tests {
    use super::{FloatText, float_text};

    /// The Reference, "Floating-point literal expressions": the suffix and the underscores are
    /// removed and the rest "is converted to the expression's type as if by `f32::from_str`
    /// or `f64::from_str`". Each expectation is the IEEE 754 binary64 and binary32 value
    /// nearest the decimal, written out in hex (0.1 is 0x1.999999999999a p-4 and
    /// 0x1.99999a p-4; 1e-3 is 0x1.0624dd2f1a9fc p-10 and 0x1.0624de p-10; 2.5 and 1000.5 are
    /// exact in both).
    ///
    /// Prints: `test front2::program::float_text::tests::float_texts_round_once_to_each_type ... ok`
    #[test]
    fn float_texts_round_once_to_each_type() {
        let bits = |wide, narrow| FloatText::Bits { wide, narrow };
        assert_eq!(float_text("0.1f32"), bits(0x3FB9_9999_9999_999A, 0x3DCC_CCCD));
        assert_eq!(float_text("0.1"), bits(0x3FB9_9999_9999_999A, 0x3DCC_CCCD));
        assert_eq!(float_text("1e-3"), bits(0x3F50_624D_D2F1_A9FC, 0x3A83_126F));
        assert_eq!(float_text("1E-3_f64"), bits(0x3F50_624D_D2F1_A9FC, 0x3A83_126F));
        assert_eq!(float_text("2.5"), bits(0x4004_0000_0000_0000, 0x4020_0000));
        assert_eq!(float_text("1_000.5"), bits(0x408F_4400_0000_0000, 0x447A_2000));
        assert_eq!(float_text("1f32"), bits(0x3FF0_0000_0000_0000, 0x3F80_0000));
        assert_eq!(float_text("2."), bits(0x4000_0000_0000_0000, 0x4000_0000));
        assert_eq!(float_text("0.0"), bits(0, 0));
        // 16777217 = 2^24 + 1: exact as `f64`, a tie as `f32` that rounds to even (2^24).
        assert_eq!(float_text("16777217.0"), bits(0x4170_0000_1000_0000, 0x4B80_0000));
    }

    /// core's `default_impl! { f16, 0.0f16, .. }` and `{ f128, 0.0f128, .. }` (library/core
    /// default.rs) are the literals that ended the walk's survey: each has no value in the
    /// seam's form and is named, never unread.
    ///
    /// Prints: `test front2::program::float_text::tests::f16_and_f128_are_named_not_unread ... ok`
    #[test]
    fn f16_and_f128_are_named_not_unread() {
        assert_eq!(float_text("0.0f16"), FloatText::NoSeamForm("f16"));
        assert_eq!(float_text("0.0f128"), FloatText::NoSeamForm("f128"));
        assert_eq!(float_text("1_f16"), FloatText::NoSeamForm("f16"));
        assert_eq!(float_text("1e3_f128"), FloatText::NoSeamForm("f128"));
        assert_eq!(float_text("f16"), FloatText::Unread);
        assert_eq!(float_text("1.0q"), FloatText::Unread);
    }
}
