//! Plain records as bytes and back, the image's only casts. A plain record ([`Record::PLAIN`])
//! is `#[repr(C)]` with no padding and every bit pattern a value, so its bytes are always
//! initialized and any aligned bytes of its size are one.

use std::mem::{align_of, size_of, size_of_val};

use blaze_format::Record;

/// The bytes of `values`.
pub fn bytes_of<T: Record>(values: &[T]) -> &[u8] {
    // SAFETY: `T: Record` has no padding (its contract), so every one of the
    // `size_of_val(values)` bytes is initialized, and they live as long as `values`.
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(values)) }
}

/// `bytes` as `T`s, `None` when they are not aligned for `T` or not a whole number of them.
pub fn cast<T: Record>(bytes: &[u8]) -> Option<&[T]> {
    const { assert!(T::PLAIN, "only a plain record is read from an image in place") };
    if bytes.is_empty() && size_of::<T>() != 0 {
        // An empty slice may sit at any address (`&[]` is at 1), and zero `T`s are never misaligned.
        return Some(&[]);
    }
    if size_of::<T>() == 0
        || !bytes.len().is_multiple_of(size_of::<T>())
        || !(bytes.as_ptr() as usize).is_multiple_of(align_of::<T>())
    {
        return None;
    }
    // SAFETY: `bytes` is aligned for `T` and holds `len / size_of::<T>()` whole `T`s (checked
    // above), and `T` is plain (asserted above): every bit pattern of its size is a `T`. The
    // result borrows `bytes`, so it lives no longer than they do.
    Some(unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<T>(), bytes.len() / size_of::<T>()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn casts_round_trip_and_refuse_misalignment() {
        let words: Vec<u64> = (0..64).map(|it| it * 0x0101_0101_0101_0101).collect();
        let bytes = bytes_of(&words);
        assert_eq!(bytes.len(), 64 * 8);
        assert_eq!(cast::<u64>(bytes), Some(&words[..]));
        assert_eq!(cast::<u32>(bytes).map(<[u32]>::len), Some(128));
        assert_eq!(cast::<u64>(&bytes[1..9]), None, "misaligned");
        assert_eq!(cast::<u64>(&bytes[..12]), None, "not whole");
        assert_eq!(cast::<u8>(&bytes[3..7]), Some(&bytes[3..7]));
        assert_eq!(cast::<u64>(&[]), Some(&[][..]), "none of them, wherever the slice is");
        assert_eq!(cast::<u64>(&bytes[1..1]), Some(&[][..]));
    }
}
