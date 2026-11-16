//! x-adt-06: fieldless enums cast to integers give their discriminants, explicit and implicit.
//!
//! The Reference, "Enumerations", "Discriminants": "If the discriminant of the first variant is
//! not specified, it is set to zero."; "If the discriminant of a variant is not explicitly
//! specified, it is set to one higher than the discriminant of the previous variant in the
//! declaration." "Type cast expressions", "Enum cast": "Casts an enum to its discriminant,
//! then uses a numeric cast if needed."
//! 1. F { X, Y, Z }: 0, 1, 2.
//! 2. E { A = 1, B = 5, C, D = -2, G }: A 1, B 5, C = 5 + 1 = 6, D -2, G = -2 + 1 = -1.
//! 3. #[repr(u8)] H { P = 255, Q = 7 }: P as u8 = 255; P as i8: 255's bits read signed = -1;
//!    P as u32 = 255 (zero-extended from u8).
//! 4. a discriminant chosen at run time: pick(true) is E::C: as i64 = 6; pick(false) E::D: -2.
//! A wrong builder that numbers variants by position exits 2 (B as 1).

enum F {
    X,
    Y,
    Z,
}

enum E {
    A = 1,
    B = 5,
    C,
    D = -2,
    G,
}

#[repr(u8)]
enum H {
    P = 255,
    Q = 7,
}

fn pick(c: bool) -> E {
    if c { E::C } else { E::D }
}

fn main() -> std::entry::Status {
    let status = if F::X as i32 != 0 || F::Y as i32 != 1 || F::Z as i32 != 2 {
        1
    } else if E::A as i32 != 1 || E::B as i32 != 5 || E::C as i32 != 6 || E::D as i32 != -2 || E::G as i32 != -1 {
        2
    } else if H::P as u8 != 255 || H::P as i8 != -1 || H::P as u32 != 255 || H::Q as u8 != 7 {
        3
    } else if pick(true) as i64 != 6 || pick(false) as i64 != -2 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
