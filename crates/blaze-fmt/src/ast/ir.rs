//! The small enums of `compiler/rustc_ast_ir/src/lib.rs` at rust-lang/rust 12433c307 (MIT or
//! Apache-2.0) that the syntax tree uses.

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Mutability {
    Not,
    Mut,
}

impl Mutability {
    pub fn prefix_str(self) -> &'static str {
        match self {
            Mutability::Mut => "mut ",
            Mutability::Not => "",
        }
    }

    pub fn ptr_str(self) -> &'static str {
        match self {
            Mutability::Not => "const",
            Mutability::Mut => "mut",
        }
    }

    pub fn is_mut(self) -> bool {
        matches!(self, Mutability::Mut)
    }

    pub fn is_not(self) -> bool {
        matches!(self, Mutability::Not)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Movability {
    Static,
    Movable,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Pinnedness {
    Not,
    Pinned,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum IntTy {
    Isize,
    I8,
    I16,
    I32,
    I64,
    I128,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum UintTy {
    Usize,
    U8,
    U16,
    U32,
    U64,
    U128,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FloatTy {
    F16,
    F32,
    F64,
    F128,
}

impl IntTy {
    pub fn name_str(&self) -> &'static str {
        match *self {
            IntTy::Isize => "isize",
            IntTy::I8 => "i8",
            IntTy::I16 => "i16",
            IntTy::I32 => "i32",
            IntTy::I64 => "i64",
            IntTy::I128 => "i128",
        }
    }
}

impl UintTy {
    pub fn name_str(&self) -> &'static str {
        match *self {
            UintTy::Usize => "usize",
            UintTy::U8 => "u8",
            UintTy::U16 => "u16",
            UintTy::U32 => "u32",
            UintTy::U64 => "u64",
            UintTy::U128 => "u128",
        }
    }
}

impl FloatTy {
    pub fn name_str(&self) -> &'static str {
        match *self {
            FloatTy::F16 => "f16",
            FloatTy::F32 => "f32",
            FloatTy::F64 => "f64",
            FloatTy::F128 => "f128",
        }
    }
}
