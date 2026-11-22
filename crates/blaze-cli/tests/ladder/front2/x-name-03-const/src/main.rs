//! x-name-03: a bare constant name in a pattern compares against the constant.
//!
//! The Reference, "Identifier patterns": "Path patterns take precedence over identifier
//! patterns." and "Path patterns": "Unqualified path patterns can refer to: enum variants,
//! structs, constants, associated constants"; a constant pattern matches a value equal to the
//! constant.
//! 1. limit(3): `LIMIT` (10) does not match 3; `ZERO` (0) does not; `n` binds: n * 2 = 6.
//! 2. limit(10): 100.       3. limit(0): 200.
//! 4. An associated constant through `Self`: `Self::MAX_LEN` (4) in `Buf::fits`: fits(4) is
//!    1, fits(5) is 0.
//! A WRONG builder that reads `LIMIT` as a binding takes the first arm: exits 1 (100).

const LIMIT: u32 = 10;
const ZERO: u32 = 0;

fn limit(x: u32) -> u32 {
    match x {
        LIMIT => 100,
        ZERO => 200,
        n => n * 2,
    }
}

struct Buf;

impl Buf {
    const MAX_LEN: u32 = 4;
    fn fits(x: u32) -> u32 {
        match x {
            Self::MAX_LEN => 1,
            _ => 0,
        }
    }
}

fn main() -> std::entry::Status {
    let status = if limit(3) != 6 {
        1
    } else if limit(10) != 100 {
        2
    } else if limit(0) != 200 {
        3
    } else if Buf::fits(4) != 1 || Buf::fits(5) != 0 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
