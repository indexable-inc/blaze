//! The encoder and decoder as they were before the encoder wrote in place and the decoder read
//! in one pass, kept as the oracle the differential tests hold the codec to: the same bytes for
//! every value, and the same value or the same error for any bytes. Never on a real path.

mod de;
mod ser;

pub use de::from_slice;
pub use ser::to_vec;

use super::{
    ARRAY, BYTES, DOUBLE, Error, FALSE, HALF, MAP, NAN, NEGATIVE, NULL, RAW, SIMPLE, SINGLE, TEXT,
    TRUE, UNSIGNED, check, from_half, half_of,
};

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let major = major << 5;
    match n {
        0..=23 => out.push(major | n as u8),
        24..=0xff => out.extend([major | 24, n as u8]),
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend((n as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend((n as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend(n.to_be_bytes());
        }
    }
}

fn float(out: &mut Vec<u8>, value: f64) {
    if value.is_nan() {
        out.push(HALF);
        out.extend(NAN.to_be_bytes());
        return;
    }
    let single = value as f32;
    if f64::from(single).to_bits() != value.to_bits() {
        out.push(DOUBLE);
        out.extend(value.to_bits().to_be_bytes());
    } else if let Some(half) = half_of(single) {
        out.push(HALF);
        out.extend(half.to_be_bytes());
    } else {
        out.push(SINGLE);
        out.extend(single.to_bits().to_be_bytes());
    }
}
