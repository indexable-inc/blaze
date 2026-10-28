//! The shape of a function's IR with its constants masked, for `harness/shapes.py` (with
//! `BLAZE_SHAPES`): two functions of one shape differ only in the constants they materialise,
//! the data they take the address of and the functions whose address they take, as Relax's
//! `merge_similar` (makepad, `libs/relax/backend/src/merge.rs`) groups them.

use std::hash::{Hash, Hasher};

use cranelift_codegen::ir::{self, ExternalName, GlobalValueData, InstructionData};

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub(super) enum Const {
    Int(i64),
    Float(u64),
    /// A request of the function (`namespace`, `index`) and an offset from it.
    Name(u32, u32, i64),
    Other(String),
}

#[derive(PartialEq, Eq, Hash, Debug)]
pub(super) struct Masked {
    pub(super) hash: u64,
    pub(super) insts: u32,
    pub(super) consts: Vec<Const>,
    /// The callee of each direct call, in the order of the calls.
    pub(super) calls: Vec<Const>,
}

fn name(func: &ir::Function, name: &ExternalName, offset: i64) -> Const {
    match name {
        ExternalName::User(r) => {
            let it = &func.params.user_named_funcs()[*r];
            Const::Name(it.namespace, it.index, offset)
        }
        other => Const::Other(format!("{}+{offset}", other.display(None))),
    }
}

/// `text` with each `fnN` token as `fn`.
fn without_func_refs(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let starts = text[i..].starts_with("fn")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_')
            && bytes.get(i + 2).is_some_and(u8::is_ascii_digit);
        if starts {
            out.push_str("fn");
            i += 2;
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

pub(super) fn masked(func: &ir::Function) -> Masked {
    let mut h = rustc_hash::FxHasher::default();
    let mut consts = Vec::new();
    let mut calls = Vec::new();
    let mut insts = 0;
    func.signature.to_string().hash(&mut h);
    for (_, slot) in func.sized_stack_slots.iter() {
        slot.to_string().hash(&mut h);
    }
    for (_, sig) in func.dfg.signatures.iter() {
        sig.to_string().hash(&mut h);
    }
    for (_, ext) in func.dfg.ext_funcs.iter() {
        (ext.signature, ext.colocated).hash(&mut h);
    }
    for (_, data) in func.dfg.constants.iter() {
        data.to_string().hash(&mut h);
    }
    for block in func.layout.blocks() {
        block.hash(&mut h);
        for &param in func.dfg.block_params(block) {
            (param, func.dfg.value_type(param).to_string()).hash(&mut h);
        }
        for inst in func.layout.block_insts(block) {
            insts += 1;
            let text = func.dfg.display_inst(inst).to_string();
            let masked = |text: &str| text.rsplit_once(' ').map_or("", |it| it.0).to_owned();
            let text = match func.dfg.insts[inst] {
                InstructionData::UnaryImm { imm, .. } => {
                    consts.push(Const::Int(imm.bits()));
                    masked(&text)
                }
                InstructionData::BinaryImm64 { imm, .. }
                | InstructionData::IntCompareImm { imm, .. } => {
                    consts.push(Const::Int(imm.bits()));
                    masked(&text)
                }
                InstructionData::UnaryIeee32 { imm, .. } => {
                    consts.push(Const::Float(imm.bits().into()));
                    masked(&text)
                }
                InstructionData::UnaryIeee64 { imm, .. } => {
                    consts.push(Const::Float(imm.bits()));
                    masked(&text)
                }
                InstructionData::UnaryGlobalValue { global_value, .. } => {
                    match &func.global_values[global_value] {
                        GlobalValueData::Symbol { name: n, offset, .. } => {
                            consts.push(name(func, n, offset.bits()));
                            masked(&text)
                        }
                        other => format!("{text} {other}"),
                    }
                }
                InstructionData::FuncAddr { func_ref, .. } => {
                    consts.push(name(func, &func.dfg.ext_funcs[func_ref].name, 0));
                    masked(&text)
                }
                InstructionData::Call { func_ref, .. }
                | InstructionData::TryCall { func_ref, .. } => {
                    calls.push(name(func, &func.dfg.ext_funcs[func_ref].name, 0));
                    without_func_refs(&text)
                }
                _ => text,
            };
            text.hash(&mut h);
        }
    }
    Masked { hash: h.finish(), insts, consts, calls }
}
