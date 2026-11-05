//! Whether bytes are one item in the core deterministic encoding.

use super::{ARRAY, BYTES, Error, MAP, NAN, SIMPLE, TEXT, from_half, half_of};

/// The deepest nesting read: a peer's bytes do not get to exhaust the stack.
pub(super) const DEEPEST: u32 = 256;

/// `Ok` when `bytes` are exactly one item in RFC 8949 section 4.2.1's core deterministic
/// encoding with what this crate writes: shortest heads, definite lengths, map keys in
/// strictly increasing bytewise order, floats in the shortest precision that holds them,
/// NaN as `f9 7e00`, UTF-8 text, no tags, and of the simple values false, true and null.
/// Otherwise the first offence and its offset.
pub fn check(bytes: &[u8]) -> Result<(), Error> {
    let mut at = 0;
    item(bytes, &mut at, 0)?;
    if at != bytes.len() {
        return Err(Error::NotDeterministic { at, what: "bytes after the item" });
    }
    Ok(())
}

/// Checks the item at `at`, `depth` deep, and moves `at` past it.
pub(super) fn item(bytes: &[u8], at: &mut usize, depth: u32) -> Result<(), Error> {
    let start = *at;
    let offence = |what| Err(Error::NotDeterministic { at: start, what });
    if depth > DEEPEST {
        return offence("items nested deeper than 256");
    }
    let Some(&first) = bytes.get(start) else { return offence("the item is cut short") };
    *at += 1;
    let (major, info) = (first >> 5, first & 0x1f);
    let n = match info {
        0..=23 => u64::from(info),
        24..=27 => {
            let arg = take(bytes, at, 1 << (info - 24))?;
            let n = arg.iter().fold(0, |n, &byte| (n << 8) | u64::from(byte));
            let shortest = match info {
                24 => n >= 24,
                25 => n > 0xff,
                26 => n > 0xffff,
                _ => n > 0xffff_ffff,
            };
            if major != SIMPLE && !shortest {
                return offence("an argument in more bytes than it needs");
            }
            n
        }
        28..=30 => return offence("a reserved head"),
        _ => return offence("an indefinite length"),
    };
    match major {
        BYTES => {
            take(bytes, at, n)?;
        }
        TEXT => {
            if std::str::from_utf8(take(bytes, at, n)?).is_err() {
                return offence("text not UTF-8");
            }
        }
        ARRAY => {
            for _ in 0..n {
                item(bytes, at, depth + 1)?;
            }
        }
        MAP => {
            let mut last: Option<&[u8]> = None;
            for _ in 0..n {
                let key = *at;
                item(bytes, at, depth + 1)?;
                let key = &bytes[key..*at];
                match last {
                    Some(last) if last == key => {
                        return Err(Error::NotDeterministic { at: *at, what: "a key twice" });
                    }
                    Some(last) if last > key => {
                        let what = "map keys out of order";
                        return Err(Error::NotDeterministic { at: *at, what });
                    }
                    _ => last = Some(key),
                }
                item(bytes, at, depth + 1)?;
            }
        }
        6 => return offence("a tag"),
        SIMPLE => match info {
            20..=22 => {}
            25 => {
                if from_half(n as u16).is_nan() && n != u64::from(NAN) {
                    return offence("a NaN other than f9 7e00");
                }
            }
            26 => {
                let single = f32::from_bits(n as u32);
                if single.is_nan() || half_of(single).is_some() {
                    return offence("a single that a half holds");
                }
            }
            27 => {
                let double = f64::from_bits(n);
                if double.is_nan() || f64::from(double as f32).to_bits() == n {
                    return offence("a double that a single holds");
                }
            }
            _ => return offence("a simple value other than false, true and null"),
        },
        _ => {}
    }
    Ok(())
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, len: u64) -> Result<&'a [u8], Error> {
    let left = bytes.len() - *at;
    match usize::try_from(len) {
        Ok(len) if len <= left => {
            *at += len;
            Ok(&bytes[*at - len..*at])
        }
        _ => Err(Error::NotDeterministic { at: *at, what: "a length past the end" }),
    }
}
