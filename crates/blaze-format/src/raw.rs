//! The format's unsafe code, behind a safe API (docs/v2/plan/design/unsafe.md): records viewed
//! in place in a buffer, and the bytes of records. The safe version would copy every record a
//! reader touches out of the buffer, which is what "the bytes in memory are the bytes on disk"
//! exists to avoid: a mapped buffer is read where it sits.
//!
//! Every view checks before it casts: size and alignment here, the value by [`Record::check`],
//! whose contract (`unsafe trait Record`) makes what it accepts a valid value.

use std::mem::{align_of, size_of, size_of_val};

use crate::{ALIGN, Block, FormatError, Record, Rel, Slice};

/// A type every bit pattern of whose size is a value, viewed in place without a check of its
/// own: a [`Rel`] or a [`Slice`] (an `i32`, a `u32` and a zero-sized marker), whose targets
/// are checked when followed.
///
/// # Safety
///
/// Every `size_of::<Self>()` bytes at `align_of::<Self>()` are a valid `Self`.
pub(crate) unsafe trait AnyBits: Sized {}

// SAFETY: `#[repr(C)]` of an `i32` and a `PhantomData`: every 4 bytes are one.
unsafe impl<T> AnyBits for Rel<T> {}
// SAFETY: `#[repr(C)]` of an `i32`, a `u32` and a `PhantomData`: every 8 bytes are one.
unsafe impl<T> AnyBits for Slice<T> {}

fn offset_in(bytes: &[u8], buffer: &[u8]) -> u64 {
    (bytes.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize) as u64
}

fn fits<T>(bytes: &[u8], len: usize, buffer: &[u8]) -> Result<(), FormatError> {
    let size = size_of::<T>().checked_mul(len);
    match size == Some(bytes.len()) && (bytes.as_ptr() as usize).is_multiple_of(align_of::<T>()) {
        true => Ok(()),
        false => Err(FormatError::Invalid {
            at: offset_in(bytes, buffer),
            what: "a record of the wrong size or alignment",
        }),
    }
}

/// The bytes of `values`.
pub(crate) fn bytes_of<T: Record>(values: &[T]) -> &[u8] {
    // SAFETY: `values` is `size_of_val(values)` bytes, all initialised since `Record`'s
    // contract rules out padding; `u8` needs no alignment, and the bytes borrow `values`.
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(values)) }
}

/// `bytes`, a part of `buffer`, as a `T`, once [`Record::check`] accepts them when `DEEP`
/// (with all they reach: what [`crate::open`] runs), else [`Record::check_shallow`] (which
/// a [`Record::SHALLOW_PLAIN`] `T` needs none of).
pub(crate) fn view<'a, T: Record, const DEEP: bool>(bytes: &'a [u8], buffer: &[u8]) -> Result<&'a T, FormatError> {
    fits::<T>(bytes, 1, buffer)?;
    match DEEP {
        true => T::check(bytes, buffer)?,
        false if !T::SHALLOW_PLAIN => T::check_shallow(bytes, buffer)?,
        false => {}
    }
    // SAFETY: `fits` checked `bytes` is `size_of::<T>()` bytes at `T`'s alignment, and
    // `T::check` or `T::check_shallow` accepted them, or `T::SHALLOW_PLAIN` makes any of them
    // one, which `Record`'s contract makes a valid `T`; the reference borrows `bytes`.
    Ok(unsafe { &*bytes.as_ptr().cast::<T>() })
}

/// `bytes`, a part of `buffer`, as `len` `T`s, once each is checked as [`view`] checks one
/// (or at once for a [`Record::PLAIN`] `T`, or one [`Record::SHALLOW_PLAIN`] when not `DEEP`,
/// which any bytes are).
pub(crate) fn view_slice<'a, T: Record, const DEEP: bool>(
    bytes: &'a [u8],
    len: usize,
    buffer: &[u8],
) -> Result<&'a [T], FormatError> {
    fits::<T>(bytes, len, buffer)?;
    let size = size_of::<T>();
    match DEEP {
        true if !T::PLAIN => (0..len).try_for_each(|i| crate::check_field::<T>(bytes, i * size, buffer))?,
        false if !T::SHALLOW_PLAIN => {
            (0..len).try_for_each(|i| crate::check_field_shallow::<T>(bytes, i * size, buffer))?
        }
        _ => {}
    }
    // SAFETY: `fits` checked `bytes` is `len` `T`s long, the first at `T`'s alignment and so
    // each (a size is a multiple of its alignment), and each is a valid `T`: checked above, or
    // `T::PLAIN`, or `T::SHALLOW_PLAIN` (any bits, references unfollowed) when not `DEEP`. The
    // slice borrows `bytes`.
    Ok(unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<T>(), len) })
}

/// `bytes`, a part of `buffer`, as an `H`, which any bytes of its size are.
pub(crate) fn in_place<'a, H: AnyBits>(bytes: &'a [u8], buffer: &[u8]) -> Result<&'a H, FormatError> {
    fits::<H>(bytes, 1, buffer)?;
    // SAFETY: `fits` checked size and alignment, and `AnyBits` makes every such bytes an `H`.
    Ok(unsafe { &*bytes.as_ptr().cast::<H>() })
}

/// The first `len` bytes of `blocks`.
pub(crate) fn blocks_bytes(blocks: &[Block], len: usize) -> &[u8] {
    assert!(len <= blocks.len() * ALIGN, "{len} bytes of {} blocks", blocks.len());
    // SAFETY: a `Block` is `ALIGN` initialised bytes with no padding, so `blocks` holds at
    // least `len` of them (asserted above) in a row; the bytes borrow `blocks`.
    unsafe { std::slice::from_raw_parts(blocks.as_ptr().cast::<u8>(), len) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Aligned;

    /// The safe reference for each view: the values decoded a byte at a time.
    fn words(bytes: &[u8]) -> Vec<u32> {
        bytes.chunks_exact(4).map(|it| u32::from_le_bytes([it[0], it[1], it[2], it[3]])).collect()
    }

    #[test]
    fn views_read_what_a_byte_at_a_time_decode_reads() {
        let values: Vec<u32> = (0..100u32).map(|n| n.wrapping_mul(2_654_435_761)).collect();
        let bytes = bytes_of(&values);
        let le: Vec<u8> = values.iter().flat_map(|it| it.to_le_bytes()).collect();
        assert_eq!(bytes, &le[..]);
        let buffer = Aligned::copy_from(bytes);
        for len in [0, 1, 7, 100] {
            assert_eq!(view_slice::<u32, true>(&buffer[..4 * len], len, &buffer).unwrap(), &words(&buffer[..4 * len])[..]);
            assert_eq!(view_slice::<u32, false>(&buffer[..4 * len], len, &buffer).unwrap(), &words(&buffer[..4 * len])[..]);
        }
        assert_eq!(*view::<u32, true>(&buffer[8..12], &buffer).unwrap(), words(&buffer[8..12])[0]);
        assert_eq!(*view::<u32, false>(&buffer[8..12], &buffer).unwrap(), words(&buffer[8..12])[0]);
        assert_eq!(*view::<u64, true>(&buffer[8..16], &buffer).unwrap(), u64::from_le_bytes(buffer[8..16].try_into().unwrap()));
        let slice = in_place::<Slice<u32>>(&buffer[16..24], &buffer).unwrap();
        assert_eq!((slice.offset, slice.len), (words(&buffer[16..20])[0] as i32, words(&buffer[20..24])[0]));
    }

    #[test]
    fn a_view_of_the_wrong_size_or_alignment_is_refused() {
        let buffer = Aligned::copy_from(&[0; 64]);
        assert!(view::<u32, true>(&buffer[1..5], &buffer).is_err());
        assert!(view::<u32, false>(&buffer[1..5], &buffer).is_err());
        assert!(view::<u32, true>(&buffer[0..3], &buffer).is_err());
        assert!(view::<u64, false>(&buffer[4..12], &buffer).is_err());
        assert!(view_slice::<u32, true>(&buffer[0..8], 3, &buffer).is_err());
        assert!(view_slice::<u32, false>(&buffer[2..10], 2, &buffer).is_err());
        assert!(view_slice::<u32, false>(&buffer[0..8], usize::MAX, &buffer).is_err());
        assert!(view_slice::<Slice<u32>, false>(&buffer[2..10], 1, &buffer).is_err(), "a Slice off its alignment");
        assert!(in_place::<Rel<u32>>(&buffer[2..6], &buffer).is_err());
        let two = Aligned::copy_from(&[2]);
        assert!(view::<bool, true>(&two, &two).is_err(), "a bool of 2");
        assert!(view::<bool, false>(&two, &two).is_err(), "a bool of 2, shallow");
    }

    #[test]
    fn blocks_give_their_first_bytes() {
        let bytes: Vec<u8> = (0..=255).collect();
        for len in [0, 1, 15, 16, 17, 255] {
            assert_eq!(&*Aligned::copy_from(&bytes[..len]), &bytes[..len]);
        }
    }
}
