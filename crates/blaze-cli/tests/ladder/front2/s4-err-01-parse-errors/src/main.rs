//! s4-err-01: `FromStr` errors and their `Display` text, from the pinned library's source:
//! - `ParseIntError` (core/src/num/error.rs): "cannot parse integer from empty string",
//!   "invalid digit found in string", "number too large to fit in target type",
//!   "number too small to fit in target type"; an unsigned type takes no leading `-` (it is an
//!   invalid digit), a leading `+` is accepted;
//! - `ParseFloatError` (core/src/num/float_parse.rs): "invalid float literal";
//! - `ParseBoolError` (core/src/str/error.rs): "provided string was not `true` or `false`";
//! - `ParseCharError` (core/src/char/convert.rs): "too many characters in string";
//! - `Utf8Error` (core/src/str/error.rs): "invalid utf-8 sequence of {} bytes from index {}".

fn main() {
    for s in ["42", "", "4x", "300", "-1"] {
        match s.parse::<u8>() {
            Ok(n) => println!("{s:?} -> {n}"),
            Err(e) => println!("{s:?} -> {e}"),
        }
    }
    println!("{:?}", "-129".parse::<i8>().map_err(|e| e.to_string()));
    println!("{:?}", "3.5".parse::<f64>());
    println!("{}", "x".parse::<f64>().unwrap_err());
    println!("{:?}", "true".parse::<bool>());
    println!("{}", "yes".parse::<bool>().unwrap_err());
    println!("{:?}", "ab".parse::<char>().map_err(|e| e.to_string()));
    println!("{}", std::str::from_utf8(&[0x61, 0xff]).unwrap_err());
    println!("{:?}", i32::from_str_radix("-ff", 16));
    println!("{:?}", "+7".parse::<u32>());
}
