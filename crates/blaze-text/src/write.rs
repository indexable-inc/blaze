//! [`Writes`]: the chainable writers, on every [`Put`].

use crate::float::Float;
use crate::int::{Digits, Integer, decimal, nibbles, radix_pow2_into, table};
use crate::put::{Count, Put};
use crate::show::Show;

/// The writers. Each appends to the sink and returns it, so a line reads left to right:
///
/// ```text
/// out.str("built ").int(n).str(" items in ").duration(ns).byte(b'\n');
/// ```
pub trait Writes: Put {
    /// `text` as it is.
    #[inline(always)]
    fn str(&mut self, text: &str) -> &mut Self {
        self.put_str(text);
        self
    }

    /// One ASCII byte (panics on a non-ASCII one: use [`Writes::char`]).
    #[inline(always)]
    fn byte(&mut self, byte: u8) -> &mut Self {
        self.put_byte(byte);
        self
    }

    #[inline]
    fn char(&mut self, c: char) -> &mut Self {
        let mut bytes = [0u8; 4];
        self.put_str(c.encode_utf8(&mut bytes));
        self
    }

    /// `true` or `false`.
    #[inline]
    fn bool(&mut self, b: bool) -> &mut Self {
        self.put_str(if b { "true" } else { "false" });
        self
    }

    /// Decimal, as `{}`.
    #[inline]
    fn int<I: Integer>(&mut self, x: I) -> &mut Self {
        self.put_str(decimal(x).as_str());
        self
    }

    /// Decimal right-aligned in `width` characters, `fill` before it, as `{:>width}` with a
    /// fill (`int_padded(n, 6, ' ')`).
    #[inline]
    fn int_padded<I: Integer>(&mut self, x: I, width: usize, fill: char) -> &mut Self {
        let digits = decimal(x);
        self.repeat(fill, width.saturating_sub(digits.len()));
        self.put_str(digits.as_str());
        self
    }

    /// Decimal zero-padded to `width` characters after the sign, as `{:0width}` (`-0042`).
    #[inline]
    fn int_zeros<I: Integer>(&mut self, x: I, width: usize) -> &mut Self {
        let mut digits = crate::int::Decimal::at_end();
        x.decimal_into(&mut digits);
        let sign = usize::from(x.negative());
        if x.negative() {
            self.put_byte(b'-');
        }
        self.repeat('0', width.saturating_sub(digits.len() + sign));
        self.put_str(digits.as_str());
        self
    }

    /// Lowercase hex with no prefix, as `{:x}` (two's complement for a negative value).
    #[inline]
    fn hex<I: Integer>(&mut self, x: I) -> &mut Self {
        self.radix(x, 4, false, 1)
    }

    /// Uppercase hex, as `{:X}`.
    #[inline]
    fn hex_upper<I: Integer>(&mut self, x: I) -> &mut Self {
        self.radix(x, 4, true, 1)
    }

    /// Lowercase hex, zero-padded to `width` digits, as `{:0width$x}` (`hex_fixed(b, 2)`).
    #[inline]
    fn hex_fixed<I: Integer>(&mut self, x: I, width: usize) -> &mut Self {
        self.radix(x, 4, false, width)
    }

    /// Uppercase hex, zero-padded to `width` digits, as `{:0width$X}` (`U+` and
    /// `hex_upper_fixed(c, 4)`).
    #[inline]
    fn hex_upper_fixed<I: Integer>(&mut self, x: I, width: usize) -> &mut Self {
        self.radix(x, 4, true, width)
    }

    /// Every hex digit of the type's width: `hex_full(7u32)` is `00000007`.
    #[inline]
    fn hex_full<I: Integer>(&mut self, x: I) -> &mut Self {
        self.radix(x, 4, false, nibbles::<I>())
    }

    /// Binary, as `{:b}`.
    #[inline]
    fn bin<I: Integer>(&mut self, x: I) -> &mut Self {
        self.radix(x, 1, false, 1)
    }

    /// Binary zero-padded to `width` digits.
    #[inline]
    fn bin_fixed<I: Integer>(&mut self, x: I, width: usize) -> &mut Self {
        self.radix(x, 1, false, width)
    }

    #[doc(hidden)]
    #[inline]
    fn radix<I: Integer>(&mut self, x: I, shift: u32, upper: bool, width: usize) -> &mut Self {
        let mut digits = Digits::at_end();
        radix_pow2_into(x.bits(), shift, table(upper), width, &mut digits);
        self.put_str(digits.as_str());
        self
    }

    /// The shortest form that reads back as the same float, laid out as `{}`: plain decimal,
    /// no exponent, no forced point: `0.1`, `1`, `-0`, `10000000000000000`, `NaN`, `inf`.
    #[inline]
    fn float<F: Float>(&mut self, x: F) -> &mut Self {
        crate::float::display(x, self);
        self
    }

    /// The same digits laid out as `{:?}`: a point always (`1.0`), an exponent outside
    /// `1e-5..1e16` (`1e16`, `1.5e-7`).
    #[inline]
    fn float_debug<F: Float>(&mut self, x: F) -> &mut Self {
        crate::float::debug(x, self);
        self
    }

    /// `x` in `{:?}`'s layout, read from its type by reflection: `Point { x: 1, y: 2 }`,
    /// `Some("a\n")`, `[1, 2]`, `(1,)`; a type's own [`crate::DebugForm`] where it has one. No
    /// derive and no bound; what reflection cannot read stops the build naming the type.
    #[inline]
    fn debug<T: ?Sized>(&mut self, x: &T) -> &mut Self {
        let mut sink: &mut Self = self;
        crate::debug::debug(x, &mut sink);
        self
    }

    /// Exactly `places` places, rounded half to even from the exact value, as `{:.places}`.
    #[inline]
    fn float_fixed<F: Float>(&mut self, x: F, places: usize) -> &mut Self {
        crate::float::fixed::fixed(x, places, self);
        self
    }

    /// Each byte as two lowercase hex digits.
    #[inline]
    fn hex_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        // Sixteen bytes at a time by a vector table lookup, the tail by the scalar table.
        let mut blocks = bytes.chunks_exact(16);
        for block in &mut blocks {
            let hex = crate::simd::hex16(block);
            match core::str::from_utf8(&hex) {
                Ok(text) => self.put_str(text),
                Err(_) => unreachable!("hex digits are ASCII"),
            }
        }
        let mut tail = crate::ascii::Ascii::<32>::at_start();
        for &b in blocks.remainder() {
            tail.back(crate::ascii::LOWER[usize::from(b >> 4)]);
            tail.back(crate::ascii::LOWER[usize::from(b & 15)]);
        }
        self.put_str(tail.as_str());
        self
    }

    /// Nanoseconds in human units: `850ns`, `1.25µs`, `12.5ms`, `1.50s`, `4m30s`, `2h05m`.
    #[inline]
    fn duration(&mut self, ns: u64) -> &mut Self {
        crate::human::duration(ns, self);
        self
    }

    /// Bytes in binary units: `512 B`, `1.50 KiB`, `12.3 MiB`.
    #[inline]
    fn byte_count(&mut self, bytes: u64) -> &mut Self {
        crate::human::byte_count(bytes, self);
        self
    }

    /// `text` as a quoted Rust string literal (`{:?}` of a `str`, ASCII controls alike).
    #[inline]
    fn escaped(&mut self, text: &str) -> &mut Self {
        self.put_byte(b'"');
        crate::escape::rust(text, self);
        self.put_byte(b'"');
        self
    }

    /// `text` as a quoted JSON string.
    #[inline]
    fn json_str(&mut self, text: &str) -> &mut Self {
        self.put_byte(b'"');
        crate::escape::json(text, self);
        self.put_byte(b'"');
        self
    }

    /// `fill` `n` times.
    #[inline]
    fn repeat(&mut self, fill: char, n: usize) -> &mut Self {
        let mut one = [0u8; 4];
        let one = fill.encode_utf8(&mut one);
        if one.len() == 1 && n > 1 {
            // Sixteen at a time.
            let mut block = crate::ascii::Ascii::<16>::at_start();
            for _ in 0..16 {
                block.back(one.as_bytes()[0]);
            }
            let mut left = n;
            while left >= 16 {
                self.put_str(block.as_str());
                left -= 16;
            }
            for _ in 0..left {
                self.put_str(one);
            }
            return self;
        }
        for _ in 0..n {
            self.put_str(one);
        }
        self
    }

    /// `value`'s [`Show`] form.
    #[inline]
    fn show<T: Show + ?Sized>(&mut self, value: &T) -> &mut Self {
        value.show(self);
        self
    }

    /// Each item's [`Show`] form, `sep` between them.
    #[inline]
    fn join<T: Show, I: IntoIterator<Item = T>>(&mut self, items: I, sep: &str) -> &mut Self {
        let mut first = true;
        for item in items {
            if !first {
                self.put_str(sep);
            }
            first = false;
            item.show(self);
        }
        self
    }

    /// `value` right-aligned in a column of `width` characters, as `{:>width}`.
    #[inline]
    fn right<T: Show + ?Sized>(&mut self, value: &T, width: usize) -> &mut Self {
        let mut count = Count::new();
        value.show(&mut count);
        self.repeat(' ', width.saturating_sub(count.chars));
        value.show(self);
        self
    }

    /// `value` left-aligned in a column of `width` characters, as `{:<width}`.
    #[inline]
    fn left<T: Show + ?Sized>(&mut self, value: &T, width: usize) -> &mut Self {
        let mut count = Count::new();
        value.show(&mut count);
        value.show(self);
        self.repeat(' ', width.saturating_sub(count.chars));
        self
    }
}

impl<P: Put + ?Sized> Writes for P {}
