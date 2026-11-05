//! An unsigned integer in decimal, as `Display` writes it, with no allocation: an HTTP status
//! and `Content-Length`, a cookie's file name, a path's numbered part.

/// `n` in decimal, as the digits it ends `digits` with.
#[inline]
pub fn decimal(mut n: u64, digits: &mut [u8; 20]) -> &str {
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&digits[at..]).expect("decimal digits are UTF-8")
}

#[cfg(test)]
mod tests {
    use hegel::TestCase;
    use hegel::generators as gs;

    use super::decimal;

    /// `blaze-core/src/fsevents.rs`' copy, which gave the bytes (`hir-ty-codegen/debuginfo.rs`
    /// had the same loop).
    fn reference(mut n: u64, digits: &mut [u8; 20]) -> &[u8] {
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                return &digits[at..];
            }
        }
    }

    #[test]
    fn numbers_are_written_in_decimal() {
        for n in [0, 7, 9, 10, 99, 100, 200, 404, 1234567890, u64::from(u32::MAX), u64::MAX] {
            assert_eq!(decimal(n, &mut [0; 20]), n.to_string());
        }
    }

    #[hegel::test]
    fn writes_what_display_and_the_copies_did(tc: TestCase) {
        let n: u64 = tc.draw(gs::integers());
        assert_eq!(decimal(n, &mut [0; 20]), n.to_string());
        assert_eq!(decimal(n, &mut [0; 20]).as_bytes(), reference(n, &mut [0; 20]));
    }
}
