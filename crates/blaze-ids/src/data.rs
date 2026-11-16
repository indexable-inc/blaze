//! [`ContentId`] and [`Fingerprint`] as blaze-data values: one byte string each, of exactly the
//! type's length (32 and 16 bytes; a fingerprint's little-endian bytes), as `wire`'s serde form
//! writes them. JSON has no bytes: a JSON writer is told how to write them (blaze-json's
//! `BytesAs`), and the reader takes the form serde_json wrote, a list of exactly that many
//! integers from 0 to 255, so a file written before the move still reads.

use core::alloc::Allocator;

use blaze_data::{Cursor, FromValue, Kind, Shape, ShapeError, ToValue, Value};

use crate::{ContentId, Fingerprint};

impl<A: Allocator + Clone> ToValue<A> for ContentId {
    fn to_value(&self, alloc: A) -> Value<A> {
        Value::Bytes(blaze_own::Vec::from_array_in(self.0, alloc))
    }
}

impl<A: Allocator + Clone> FromValue<A> for ContentId {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<ContentId, ShapeError<A>> {
        exact(c, "ContentId").map(ContentId)
    }
}

impl<A: Allocator + Clone> ToValue<A> for Fingerprint {
    fn to_value(&self, alloc: A) -> Value<A> {
        Value::Bytes(blaze_own::Vec::from_array_in(self.0.to_le_bytes(), alloc))
    }
}

impl<A: Allocator + Clone> FromValue<A> for Fingerprint {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<Fingerprint, ShapeError<A>> {
        exact(c, "Fingerprint").map(|bytes| Fingerprint(u128::from_le_bytes(bytes)))
    }
}

/// `N` bytes from a byte string of exactly `N`, or a list of exactly `N` integers that fit a byte.
fn exact<A: Allocator + Clone, const N: usize>(
    c: &Cursor<'_, '_, A>,
    ty: &'static str,
) -> Result<[u8; N], ShapeError<A>> {
    match c.value() {
        Value::Bytes(bytes) => <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| c.error(Shape::OutOfRange { ty })),
        Value::List(list) => {
            if list.len() != N {
                return Err(c.error(Shape::OutOfRange { ty }));
            }
            let mut bytes = [0u8; N];
            for (at, byte) in bytes.iter_mut().enumerate() {
                *byte = c.index(at)?.get::<u8>()?;
            }
            Ok(bytes)
        }
        _ => Err(c.expected(Kind::Bytes)),
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_data::{Cursor, Int, ToValue, Value};

    use crate::{ContentId, Fingerprint};

    /// Bytes round-trip; serde_json's old form (a list of numbers) reads; a wrong length does not.
    #[test]
    fn ids_are_byte_strings_and_read_the_old_json_form() {
        let id = ContentId::of("blaze-ids data test 1", b"x");
        let value = id.to_value(Global);
        assert_eq!(value.as_bytes(), Some(&id.0[..]));
        assert_eq!(Cursor::root(&value, Global).get::<ContentId>().unwrap(), id);
        let fingerprint = Fingerprint(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10);
        let value = fingerprint.to_value(Global);
        assert_eq!(Cursor::root(&value, Global).get::<Fingerprint>().unwrap(), fingerprint);

        let numbers = |len: usize| {
            let mut list = blaze_own::Vec::new_in(Global);
            for at in 0..len {
                list.push(Value::Int(Int::from(u64::from(id.0[at % 32]))));
            }
            Value::<Global>::List(list)
        };
        assert_eq!(Cursor::root(&numbers(32), Global).get::<ContentId>().unwrap(), id);
        for len in [0, 31, 33] {
            assert!(Cursor::root(&numbers(len), Global).get::<ContentId>().is_err(), "{len} numbers");
        }
    }
}
