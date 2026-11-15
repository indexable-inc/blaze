//! The encoder half of a hand-written codec (`crates/hir-ty/src/mir/persist.rs`, the
//! `request_index` of `crates/blaze-api/src/frames/identity.rs`): each variant's tag by hand.
//! agents/annoyances/image-part-array-positional-callers.json: `Part::DefMap` held tag 2, the
//! next free part's tag.

pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

pub enum Part {
    LangItems,
    Signatures,
    DefMap,
}

// Found: `Mul` and `Div` share tag 2.
pub fn bin_op_tag(op: &BinOp) -> u8 {
    match op {
        BinOp::Add => 0,
        BinOp::Sub => 1,
        BinOp::Mul => 2,
        BinOp::Div => 2,
    }
}

// Not found: every part its own tag.
pub fn part_tag(part: &Part) -> u8 {
    match part {
        Part::LangItems => 0,
        Part::Signatures => 1,
        Part::DefMap => 3,
    }
}

// Not found: two tables may give one variant different numbers; each is its own.
pub fn weight(op: &BinOp) -> u8 {
    match op {
        BinOp::Add | BinOp::Sub => 1,
        BinOp::Mul => 2,
        BinOp::Div => 3,
    }
}
