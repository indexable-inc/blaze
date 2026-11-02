//! The x86-64 glue against Cranelift 0.137's code of the same functions, with the flags
//! `hir-ty-codegen/target.rs` gives the dev ISA: the encodings of what the glue writes, the
//! probe, the Windows unwind codes, the unwind rows and Windows `UNWIND_INFO` of a function,
//! the relocations of calls, addresses, thread-locals, StructArgument copies and libcall
//! operations, the jump table sequence, a landing pad, and where System V and Windows put
//! each argument and return value. Our side runs the real patcher over a mock library of
//! copies, constants and jumps only, so everything else in these functions is glue.

use std::sync::OnceLock;

use cranelift_codegen::{
    Context, RelocTarget, RegClass,
    binemit::Reloc,
    control::ControlPlane,
    ir::{
        self, AbiParam, ArgumentPurpose, InstBuilder,
        immediates::Imm64,
        types::{F32, F32X4, F64, I32, I64, I128},
    },
    isa::{
        self, CallConv, OwnedTargetIsa,
        unwind::{UnwindInfo, UnwindInst},
    },
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use super::{
    X64,
    abi::{self, Part, Place},
    asm::*,
    frame::{self, Save},
};
use crate::{
    Cpu, Error, Os, Target, TlsModel,
    arch::{Area, Glue},
    buf::Buf,
    code::{self, Code, Handler, Name, Rule, Unwind},
    enc::Form,
    patch::compile_in,
    shape::{self, Dst, Isa, Op, Shape, Src},
    table::format::{Hole, HoleKind, Reader, Variant, Writer},
};

const LINUX: Target = Target {
    arch: blaze_codegen::Arch::X86_64,
    os: Os::Linux,
    cpu: Cpu::Baseline,
    pic: true,
    tls: TlsModel::ElfGd,
    probe_stack: true,
    probe_log2: 12,
};
const MACOS: Target = Target { os: Os::MacOs, tls: TlsModel::MachO, ..LINUX };
const WINDOWS: Target = Target { os: Os::Windows, pic: false, tls: TlsModel::Coff, ..LINUX };

fn conv(target: &Target) -> CallConv {
    if target.os == Os::Windows { CallConv::WindowsFastcall } else { CallConv::SystemV }
}

fn isa(target: &Target) -> OwnedTargetIsa {
    let triple = match target.os {
        Os::Linux => "x86_64-unknown-linux-gnu",
        Os::MacOs => "x86_64-apple-darwin",
        Os::Windows => "x86_64-pc-windows-msvc",
    };
    let tls = match target.tls {
        TlsModel::ElfGd => "elf_gd",
        TlsModel::MachO => "macho",
        TlsModel::Coff => "coff",
    };
    let mut flags = settings::builder();
    for (name, value) in [
        ("is_pic", if target.pic { "true" } else { "false" }),
        ("tls_model", tls),
        ("enable_probestack", "true"),
        ("probestack_strategy", "inline"),
        ("probestack_size_log2", "12"),
        ("preserve_frame_pointers", "true"),
        ("opt_level", "none"),
        ("enable_verifier", "true"),
        ("enable_llvm_abi_extensions", "true"),
    ] {
        flags.set(name, value).unwrap_or_else(|e| panic!("{name}={value}: {e}"));
    }
    let mut builder = isa::lookup_by_name(triple).unwrap_or_else(|e| panic!("{triple}: {e}"));
    builder.enable("has_cmpxchg16b").expect("has_cmpxchg16b");
    builder.finish(settings::Flags::new(flags)).expect("a cranelift ISA")
}

fn name_of(func: &ir::Function, name: &ir::ExternalName) -> Name {
    match name {
        ir::ExternalName::User(at) => {
            let name = &func.params.user_named_funcs()[*at];
            Name::User { namespace: name.namespace, index: name.index }
        }
        ir::ExternalName::LibCall(call) => Name::LibCall(*call),
        ir::ExternalName::KnownSymbol(symbol) => Name::Known(*symbol),
        ir::ExternalName::TestCase(_) => panic!("a test case's name"),
    }
}

struct Clif {
    bytes: Vec<u8>,
    relocs: Vec<code::Reloc>,
    insts: Vec<(u32, UnwindInst)>,
    unwind: Option<UnwindInfo>,
    /// `frame_to_fp_offset` of its frame layout.
    frame: u32,
}

fn cranelift(func: &ir::Function, target: &Target) -> Clif {
    let isa = isa(target);
    let mut ctx = Context::for_function(func.clone());
    let compiled = ctx
        .compile(&*isa, &mut ControlPlane::default())
        .unwrap_or_else(|e| panic!("cranelift: {:?}", e.inner));
    let relocs = compiled
        .buffer
        .relocs()
        .iter()
        .map(|reloc| code::Reloc {
            at: reloc.offset,
            kind: reloc.kind,
            name: match &reloc.target {
                RelocTarget::ExternalName(name) => name_of(func, name),
                RelocTarget::Label(at) => Name::Own(at.as_offset()),
            },
            addend: reloc.addend,
        })
        .collect();
    let insts = compiled.buffer.unwind_info.iter().cloned().collect();
    let unwind = compiled.create_unwind_info(&*isa).expect("unwind information");
    let frame = compiled.buffer.frame_layout().expect("a frame layout").frame_to_fp_offset;
    Clif { bytes: compiled.code_buffer().to_vec(), relocs, insts, unwind, frame }
}

// The mock library: each stencil a header, one field per hole and a tail jump, as the
// patcher's own tests make them.

const HEADER: u8 = 0x5c;
const OPS: [Op; 4] = [Op::Copy, Op::Iconst, Op::Fconst, Op::Jump];

fn fields(shape: Shape) -> Vec<(HoleKind, Form)> {
    let info = shape.op.info();
    let mut out = Vec::new();
    match shape.op {
        Op::Iconst | Op::Fconst => {
            out.push((HoleKind::Imm0, if shape.aux == 1 { Form::Abs64 } else { Form::Abs32S }));
        }
        _ => {
            const SLOTS: [HoleKind; 3] = [HoleKind::Slot0, HoleKind::Slot1, HoleKind::Slot2];
            const IMMS: [HoleKind; 3] = [HoleKind::Imm0, HoleKind::Imm1, HoleKind::Imm2];
            for (n, src) in shape.srcs.into_iter().enumerate() {
                match src {
                    Src::Slot => out.push((SLOTS[n], Form::Abs32S)),
                    Src::Imm => out.push((IMMS[n], Form::Abs32S)),
                    Src::Acc | Src::None => {}
                }
            }
        }
    }
    if matches!(shape.dst, Dst::Slot | Dst::Both) {
        out.push((HoleKind::Result0, Form::Abs32S));
    }
    if info.results > 1 {
        out.push((HoleKind::Result1, Form::Abs32S));
    }
    if info.branches {
        out.push((HoleKind::Target, Form::Rel32));
    }
    out
}

fn stencil(shape: Shape) -> (Vec<u8>, Vec<Hole>) {
    let mut code = vec![HEADER];
    code.extend_from_slice(&shape.key().to_le_bytes());
    let mut holes = Vec::new();
    for (kind, form) in fields(shape) {
        let addend = if form.pc_relative() { -4 } else { 0 };
        holes.push(Hole { at: code.len() as u16, form, kind, addend });
        code.resize(code.len() + usize::from(form.bytes()), 0);
    }
    code.push(0xe9);
    holes.push(Hole { at: code.len() as u16, form: Form::Rel32, kind: HoleKind::Next, addend: -4 });
    code.extend_from_slice(&[0; 4]);
    (code, holes)
}

fn mock() -> Reader<'static> {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = BYTES.get_or_init(|| {
        let mut writer = Writer::new(Variant::X86_64);
        shape::each_on(Isa::X86_64, |shape| {
            if OPS.contains(&shape.op) {
                let (code, holes) = stencil(shape);
                writer.push(shape, &code, 5, &holes).unwrap();
            }
        });
        writer.finish(|_| [0; 32]).unwrap()
    });
    Reader::new(bytes).unwrap()
}

fn compile(func: &ir::Function, target: &Target) -> Result<Code, Error> {
    compile_in::<X64, _>(func, target, &mock(), |code, _| code.to_code())
}

fn ours(func: &ir::Function, target: &Target) -> Code {
    compile(func, target).unwrap_or_else(|e| panic!("{e}"))
}

fn sig(conv: CallConv, params: &[ir::Type], rets: &[ir::Type]) -> ir::Signature {
    let mut sig = ir::Signature::new(conv);
    sig.params.extend(params.iter().map(|ty| AbiParam::new(*ty)));
    sig.returns.extend(rets.iter().map(|ty| AbiParam::new(*ty)));
    sig
}

fn build(
    sig: ir::Signature,
    body: impl FnOnce(&mut FunctionBuilder<'_>, &[ir::Value]),
) -> ir::Function {
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(0, 0), sig);
    let mut ctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut ctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let params = b.block_params(entry).to_vec();
    body(&mut b, &params);
    b.seal_all_blocks();
    b.finalize(isa(&LINUX).frontend_config());
    func
}

fn import(
    b: &mut FunctionBuilder<'_>,
    index: u32,
    sig: ir::Signature,
    colocated: bool,
) -> ir::FuncRef {
    let signature = b.import_signature(sig);
    let name = b.func.declare_imported_user_function(ir::UserExternalName::new(1, index));
    b.import_function(ir::ExtFuncData {
        name: ir::ExternalName::user(name),
        signature,
        colocated,
        patchable: false,
    })
}

fn symbol(b: &mut FunctionBuilder<'_>, index: u32, colocated: bool, tls: bool) -> ir::GlobalValue {
    let name = b.func.declare_imported_user_function(ir::UserExternalName::new(1, index));
    b.create_global_value(ir::GlobalValueData::Symbol {
        name: ir::ExternalName::user(name),
        offset: Imm64::new(0),
        colocated,
        tls,
    })
}

/// The relocations but the tier counter's, in an order both sides share.
fn relocs(relocs: &[code::Reloc]) -> Vec<(String, Name, i64)> {
    let mut out: Vec<_> = relocs
        .iter()
        .filter(|it| it.name != Name::Counter)
        .map(|it| (format!("{:?}", it.kind), it.name, it.addend))
        .collect();
    out.sort_by(|a, b| (&a.0, format!("{:?}", a.1), a.2).cmp(&(&b.0, format!("{:?}", b.1), b.2)));
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|it| it == needle)
}

fn enc(f: impl FnOnce(&mut Buf<'_>)) -> Vec<u8> {
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |temp| {
        let mut buf = Buf::new_in(temp);
        f(&mut buf);
        buf.bytes().to_vec()
    })
}

#[test]
fn encodes_as_cranelifts_assembler() {
    let cases: Vec<(Vec<u8>, &[u8])> = vec![
        (enc(|b| push(b, RBP)), &[0x55]),
        (enc(|b| push(b, R12)), &[0x41, 0x54]),
        (enc(|b| pop(b, RBP)), &[0x5d]),
        (enc(|b| mov(b, RBP, RSP)), &[0x48, 0x89, 0xe5]),
        (enc(|b| mov(b, RSP, RBP)), &[0x48, 0x89, 0xec]),
        (enc(|b| mov(b, R11, RSP)), &[0x49, 0x89, 0xe3]),
        (enc(|b| store(b, at(RSP, 8), RBX)), &[0x48, 0x89, 0x5c, 0x24, 0x08]),
        (enc(|b| store(b, at(RSP, 0x100), R12)), &[0x4c, 0x89, 0xa4, 0x24, 0x00, 0x01, 0x00, 0x00]),
        (enc(|b| load(b, RAX, at(R12, 0))), &[0x49, 0x8b, 0x04, 0x24]),
        (enc(|b| load(b, RAX, at(R13, 0))), &[0x49, 0x8b, 0x45, 0x00]),
        (enc(|b| load(b, RAX, at(RBP, 8))), &[0x48, 0x8b, 0x45, 0x08]),
        (enc(|b| load(b, R10, at(RBP, 16))), &[0x4c, 0x8b, 0x55, 0x10]),
        (enc(|b| lea(b, R12, at(RSP, 32))), &[0x4c, 0x8d, 0x64, 0x24, 0x20]),
        (enc(|b| load_ext(b, RAX, at(R12, 8), 8, false)), &[0x41, 0x0f, 0xb6, 0x44, 0x24, 0x08]),
        (enc(|b| load_ext(b, RDI, at(R12, 8), 16, true)), &[0x49, 0x0f, 0xbf, 0x7c, 0x24, 0x08]),
        (enc(|b| load_ext(b, RSI, at(R12, 8), 32, true)), &[0x49, 0x63, 0x74, 0x24, 0x08]),
        (enc(|b| load_ext(b, R8, at(R12, 8), 32, false)), &[0x45, 0x8b, 0x44, 0x24, 0x08]),
        (enc(|b| imm(b, RAX, 5)), &[0xb8, 5, 0, 0, 0]),
        (enc(|b| imm(b, R8, 5)), &[0x41, 0xb8, 5, 0, 0, 0]),
        (enc(|b| imm(b, RAX, u64::MAX)), &[0x48, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff]),
        (enc(|b| imm(b, R10, 1 << 40)), &[0x49, 0xba, 0, 0, 0, 0, 0, 1, 0, 0]),
        (enc(|b| alu_imm(b, SUB, RSP, 0x30)), &[0x48, 0x83, 0xec, 0x30]),
        (enc(|b| alu_imm(b, SUB, RSP, 0x1000)), &[0x48, 0x81, 0xec, 0x00, 0x10, 0x00, 0x00]),
        (enc(|b| alu_imm(b, ADD, RSP, 0x1000)), &[0x48, 0x81, 0xc4, 0x00, 0x10, 0x00, 0x00]),
        (enc(|b| alu_imm(b, AND, R12, -64)), &[0x49, 0x83, 0xe4, 0xc0]),
        (enc(|b| alu_imm32(b, ADD, R11, 8)), &[0x49, 0x81, 0xc3, 8, 0, 0, 0]),
        (enc(|b| store_imm32(b, at(RSP, 0), 0)), &[0xc7, 0x04, 0x24, 0, 0, 0, 0]),
        (enc(|b| cmp(b, R11, RSP)), &[0x4c, 0x3b, 0xdc]),
        (enc(|b| cmp(b, RAX, RCX)), &[0x48, 0x3b, 0xc1]),
        (enc(|b| cmov(b, CC_AE, RAX, RCX)), &[0x48, 0x0f, 0x43, 0xc1]),
        (enc(|b| add(b, RCX, RDX)), &[0x48, 0x03, 0xca]),
        (enc(|b| movsxd_index4(b, RDX, RCX, RAX)), &[0x48, 0x63, 0x14, 0x81]),
        (enc(|b| or(b, RAX, RCX)), &[0x48, 0x0b, 0xc1]),
        (enc(|b| and(b, RCX, R10)), &[0x49, 0x23, 0xca]),
        (enc(|b| sbb(b, R10, R10)), &[0x4d, 0x1b, 0xd2]),
        (enc(|b| shl_imm(b, RDX, 8)), &[0x48, 0xc1, 0xe2, 0x08]),
        (enc(|b| alu_imm(b, CMP, RCX, 16)), &[0x48, 0x83, 0xf9, 0x10]),
        (enc(|b| load_u8_index(b, RCX, R11, RCX)), &[0x41, 0x0f, 0xb6, 0x0c, 0x0b]),
        (enc(|b| jmp_reg(b, RCX)), &[0xff, 0xe1]),
        (enc(|b| call_reg(b, R11)), &[0x41, 0xff, 0xd3]),
        (enc(|b| call_mem(b, at(RDI, 0))), &[0xff, 0x17]),
        (
            enc(|b| {
                call_rel32(b);
            }),
            &[0xe8, 0, 0, 0, 0],
        ),
        (
            enc(|b| {
                add_rip_one(b);
            }),
            &[0x48, 0x83, 0x05, 0, 0, 0, 0, 1],
        ),
        (
            enc(|b| {
                rip(b, None, true, &[0x8b], R11);
            }),
            &[0x4c, 0x8b, 0x1d, 0, 0, 0, 0],
        ),
        (enc(|b| xstore(b, Sse::Dqu, at(RSP, 0x40), 6)), &[0xf3, 0x0f, 0x7f, 0x74, 0x24, 0x40]),
        (enc(|b| xstore(b, Sse::Dqu, at(RSP, 0x40), 15)), &[0xf3, 0x44, 0x0f, 0x7f, 0x7c, 0x24, 0x40]),
        (enc(|b| xload(b, Sse::Dqu, 15, at(RSP, 0x40))), &[0xf3, 0x44, 0x0f, 0x6f, 0x7c, 0x24, 0x40]),
        (enc(|b| xload(b, Sse::Ss, 0, at(R12, 8))), &[0xf3, 0x41, 0x0f, 0x10, 0x44, 0x24, 0x08]),
        (enc(|b| xstore(b, Sse::Sd, at(R12, 8), 1)), &[0xf2, 0x41, 0x0f, 0x11, 0x4c, 0x24, 0x08]),
        (enc(|b| movq_to_xmm(b, 0, R10)), &[0x66, 0x49, 0x0f, 0x6e, 0xc2]),
        (enc(|b| ret_imm(b, 0x30)), &[0xc2, 0x30, 0x00]),
        (
            enc(|b| {
                jmp_rel32(b);
            }),
            &[0xe9, 0, 0, 0, 0],
        ),
        (enc(ud2), &[0x0f, 0x0b]),
        (enc(ret), &[0xc3]),
    ];
    for (k, (got, want)) in cases.iter().enumerate() {
        assert_eq!(got.as_slice(), *want, "case {k}");
    }
}

/// A leaf whose frame is one stack slot of `bytes`: Cranelift's prologue is `push rbp`, `mov
/// rbp, rsp`, the probe of `bytes`, `sub rsp, bytes`.
fn leaf_with_slot(bytes: u32) -> ir::Function {
    build(sig(CallConv::SystemV, &[], &[]), |b, _| {
        b.create_sized_stack_slot(ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, bytes, 0));
        b.ins().return_(&[]);
    })
}

#[test]
fn probes_as_cranelift() {
    for bytes in [2048, 4096, 8192, 16384, 20480, 65536, 1 << 20] {
        let clif = cranelift(&leaf_with_slot(bytes), &LINUX);
        let probe = frame::probe_of(bytes, false, &LINUX);
        let probed = enc(|b| frame::probe(b, probe).unwrap());
        let sub = enc(|b| alu_imm(b, SUB, RSP, bytes as i32));
        let want = [&[0x55, 0x48, 0x89, 0xe5][..], &probed, &sub].concat();
        assert_eq!(&clif.bytes[..want.len()], want.as_slice(), "{bytes} bytes: {probe:?}");
    }
}

/// Cranelift's unwind instructions as the rows the glue records for the same, and the
/// distance down to the saves.
fn rows_of(insts: &[(u32, UnwindInst)]) -> (Vec<(u32, Rule)>, u32) {
    let clobber = insts
        .iter()
        .find_map(|(_, inst)| match inst {
            UnwindInst::DefineNewFrame { offset_downward_to_clobbers, .. } => {
                Some(*offset_downward_to_clobbers)
            }
            _ => None,
        })
        .expect("a frame");
    let mut rows = Vec::new();
    for (at, inst) in insts {
        match inst {
            UnwindInst::PushFrameRegs { .. } => {
                rows.push((*at, Rule::CfaOffset(16)));
                rows.push((*at, Rule::Offset(6, -16)));
            }
            UnwindInst::DefineNewFrame { .. } => rows.push((*at, Rule::CfaRegister(6))),
            UnwindInst::SaveReg { clobber_offset, reg } => {
                let save = match reg.class() {
                    RegClass::Int => Save::Gpr(reg.hw_enc()),
                    _ => Save::Xmm(reg.hw_enc()),
                };
                let cfa = *clobber_offset as i32 - 16 - clobber as i32;
                rows.push((*at, Rule::Offset(frame::dwarf(save), cfa)));
            }
            other => panic!("{other:?}"),
        }
    }
    (rows, clobber)
}

/// Integers and floats live across a call, which Cranelift keeps in callee-saved registers.
fn pressure(conv: CallConv) -> ir::Function {
    build(sig(conv, &[I64, F64], &[I64, F64]), |b, p| {
        let callee = import(b, 0, sig(conv, &[], &[]), true);
        let ints: Vec<_> = (0..10).map(|k| b.ins().iadd_imm(p[0], k)).collect();
        let floats: Vec<_> = (0..12).map(|_| b.ins().fadd(p[1], p[1])).collect();
        b.ins().call(callee, &[]);
        let int = ints[1..].iter().fold(ints[0], |sum, &it| b.ins().iadd(sum, it));
        let float = floats[1..].iter().fold(floats[0], |sum, &it| b.ins().fadd(sum, it));
        b.ins().return_(&[int, float]);
    })
}

#[test]
fn windows_codes_as_cranelift() {
    let clif = cranelift(&pressure(CallConv::WindowsFastcall), &WINDOWS);
    let (rows, clobber) = rows_of(&clif.insts);
    let saves = rows.iter().filter(|(_, rule)| matches!(rule, Rule::Offset(reg, _) if *reg != 6));
    assert!(saves.count() > 2, "Cranelift saved too little to compare: {rows:?}");
    let Some(UnwindInfo::WindowsX64(info)) = &clif.unwind else { panic!("{:?}", clif.unwind) };
    let mut want = vec![0; info.emit_size()];
    info.emit(&mut want);
    let mut raw = Vec::new();
    frame::win_raw(&rows, clobber, &mut raw).unwrap();
    assert_eq!(raw, want);
}

/// The codes of an `UNWIND_INFO`: `(operation, register, the word after, if one)`.
fn codes(info: &[u8]) -> Vec<(u8, u8, u16)> {
    let nodes = &info[4..4 + 2 * usize::from(info[2])];
    let mut out = Vec::new();
    let mut at = 0;
    while at < nodes.len() {
        let (op, reg) = (nodes[at + 1] & 0xf, nodes[at + 1] >> 4);
        let words = match op {
            0 | 2 | 3 => 0,
            1 | 4 | 8 => 1,
            op => panic!("operation {op}"),
        };
        let word = if words == 1 { u16::from_le_bytes([nodes[at + 2], nodes[at + 3]]) } else { 0 };
        out.push((op, reg, word));
        at += 2 + 2 * words;
    }
    out
}

#[test]
fn windows_info_has_every_save() {
    let func = build(sig(CallConv::WindowsFastcall, &[I64], &[I64]), |b, p| {
        b.ins().return_(&p[..1]);
    });
    let code = ours(&func, &WINDOWS);
    assert_eq!(&code.bytes[..4], &[0x55, 0x48, 0x89, 0xe5]);
    let Some(Unwind::Windows { len, info }) = &code.unwind else { panic!("{:?}", code.unwind) };
    assert_eq!(*len as usize, code.bytes.len());
    // 224 bytes of saves below `rbp`: 14 in the frame offset's nibble.
    assert_eq!((info[0], info[3]), (1, 0xe5));
    assert_eq!(info.len() % 4, 0);
    let mut want = Vec::new();
    for xmm in (6..16u8).rev() {
        want.push((8, xmm, 4 + u16::from(xmm) - 6));
    }
    for (word, reg) in [RBX, RSI, RDI, R12, R13, R14, R15].into_iter().enumerate().rev() {
        want.push((4, reg, word as u16));
    }
    want.extend([(3, 0, 0), (1, 0, 224 / 8), (0, 5, 0)]);
    assert_eq!(codes(info), want);
    // The prologue's size is where the last save ends.
    assert_eq!(info[1], info[4]);
}

#[test]
fn dwarf_rows_follow_the_frame() {
    let func = build(sig(CallConv::SystemV, &[I64], &[I64]), |b, p| {
        b.ins().return_(&p[..1]);
    });
    let code = ours(&func, &LINUX);
    let Some(Unwind::Dwarf { len, rows }) = &code.unwind else { panic!("{:?}", code.unwind) };
    assert_eq!(*len as usize, code.bytes.len());
    assert_eq!(rows[..3], [(1, Rule::CfaOffset(16)), (1, Rule::Offset(6, -16)), (4, Rule::CfaRegister(6))]);
    let saves: Vec<_> = rows[3..8].iter().map(|row| row.1).collect();
    assert_eq!(
        saves,
        [
            Rule::Offset(3, -64),
            Rule::Offset(12, -56),
            Rule::Offset(13, -48),
            Rule::Offset(14, -40),
            Rule::Offset(15, -32),
        ]
    );
    let at = rows.iter().find(|row| row.1 == Rule::RememberState).expect("the epilogue's rows").0;
    assert!(rows.contains(&(at, Rule::Cfa(7, 8))));
    // After `pop rbp`, at the `ret`.
    assert_eq!(&code.bytes[at as usize - 1..at as usize + 1], &[0x5d, 0xc3]);
    assert!(rows.iter().all(|row| row.0 < *len));
    let counter = code.relocs.iter().find(|it| it.name == Name::Counter).expect("the calls counter");
    assert_eq!((counter.kind, counter.addend), (Reloc::X86PCRel4, crate::tier::CALLS - 5));
    assert_eq!(code.slots.base, 12);
}

/// Direct calls near and far, and indirect ones through `func_addr` of each.
fn callers(conv: CallConv) -> ir::Function {
    build(sig(conv, &[I64], &[I64]), |b, p| {
        let near = import(b, 0, sig(conv, &[I64], &[I64]), true);
        let far = import(b, 1, sig(conv, &[I64], &[I64]), false);
        let one = b.ins().call(near, &[p[0]]);
        let one = b.inst_results(one)[0];
        let two = b.ins().call(far, &[one]);
        let two = b.inst_results(two)[0];
        let far_addr = b.ins().func_addr(I64, far);
        let near_addr = b.ins().func_addr(I64, near);
        let indirect = b.import_signature(sig(conv, &[I64], &[I64]));
        let three = b.ins().call_indirect(indirect, far_addr, &[two]);
        let three = b.inst_results(three)[0];
        let four = b.ins().call_indirect(indirect, near_addr, &[three]);
        let four = b.inst_results(four)[0];
        b.ins().return_(&[four]);
    })
}

#[test]
fn calls_relocate_as_cranelift() {
    for target in [LINUX, WINDOWS] {
        let func = callers(conv(&target));
        let code = ours(&func, &target);
        assert_eq!(relocs(&code.relocs), relocs(&cranelift(&func, &target).relocs), "{:?}", target.os);
        assert_eq!(code.calls.len(), 4);
    }
    let code = ours(&callers(CallConv::SystemV), &LINUX);
    let near = code.relocs.iter().find(|it| it.kind == Reloc::X86CallPCRel4).expect("a near call");
    assert_eq!((code.bytes[near.at as usize - 1], near.addend), (0xe8, -4));
}

#[test]
fn thread_locals_as_cranelift() {
    const ELF: [u8; 16] = [0x66, 0x48, 0x8d, 0x3d, 0, 0, 0, 0, 0x66, 0x66, 0x48, 0xe8, 0, 0, 0, 0];
    const MACHO: [u8; 9] = [0x48, 0x8b, 0x3d, 0, 0, 0, 0, 0xff, 0x17];
    const COFF: [u8; 26] = [
        0x8b, 0x05, 0, 0, 0, 0, 0x65, 0x48, 0x8b, 0x0c, 0x25, 0x58, 0, 0, 0, 0x48, 0x8b, 0x04,
        0xc1, 0x48, 0x8d, 0x80, 0, 0, 0, 0,
    ];
    for (target, sequence) in [(LINUX, &ELF[..]), (MACOS, &MACHO[..]), (WINDOWS, &COFF[..])] {
        let func = build(sig(conv(&target), &[], &[I64]), |b, _| {
            let gv = symbol(b, 0, false, true);
            let v = b.ins().tls_value(I64, gv);
            b.ins().return_(&[v]);
        });
        let clif = cranelift(&func, &target);
        let code = ours(&func, &target);
        assert!(find(&clif.bytes, sequence).is_some(), "Cranelift's {:?}", target.os);
        assert!(find(&code.bytes, sequence).is_some(), "ours {:?}", target.os);
        assert_eq!(relocs(&code.relocs), relocs(&clif.relocs), "{:?}", target.os);
    }
}

#[test]
fn struct_arguments_copy_as_cranelift() {
    for target in [LINUX, WINDOWS] {
        let conv = conv(&target);
        let mut callee = sig(conv, &[I64], &[]);
        callee.params.insert(0, AbiParam::special(I64, ArgumentPurpose::StructArgument(24)));
        let func = build(sig(conv, &[I64, I64], &[]), |b, p| {
            let f = import(b, 0, callee.clone(), false);
            b.ins().call(f, &[p[0], p[1]]);
            b.ins().return_(&[]);
        });
        let code = ours(&func, &target);
        assert_eq!(relocs(&code.relocs), relocs(&cranelift(&func, &target).relocs), "{:?}", target.os);
        // The `memcpy` and the call.
        assert_eq!(code.calls.len(), 2);
    }
}

#[test]
fn libcall_operations_as_cranelift() {
    for (ty, args) in [(F32, 1), (F64, 1), (F32X4, 1), (F64, 3), (F32X4, 3)] {
        let func = build(sig(CallConv::SystemV, &[ty, ty, ty], &[ty]), |b, p| {
            let v = if args == 1 { b.ins().ceil(p[0]) } else { b.ins().fma(p[0], p[1], p[2]) };
            b.ins().return_(&[v]);
        });
        let code = ours(&func, &LINUX);
        assert_eq!(relocs(&code.relocs), relocs(&cranelift(&func, &LINUX).relocs), "{ty} {args}");
        assert_eq!(code.calls.len() as u32, ty.lane_count());
    }
}

#[test]
fn jump_tables_as_cranelift() {
    let func = build(sig(CallConv::SystemV, &[I32], &[]), |b, p| {
        let targets: Vec<_> = (0..4).map(|_| b.create_block()).collect();
        let calls: Vec<_> = targets[1..].iter().map(|&it| b.func.dfg.block_call(it, &[])).collect();
        let default = b.func.dfg.block_call(targets[0], &[]);
        let table = b.create_jump_table(ir::JumpTableData::new(default, &calls));
        b.ins().br_table(p[0], table);
        for &target in &targets {
            b.switch_to_block(target);
            b.ins().return_(&[]);
        }
    });
    let code = ours(&func, &LINUX);
    let sequence = [
        0xb9, 3, 0, 0, 0, 0x48, 0x3b, 0xc1, 0x48, 0x0f, 0x43, 0xc1, 0x48, 0x8d, 0x0d, 9, 0, 0, 0,
        0x48, 0x63, 0x14, 0x81, 0x48, 0x03, 0xca, 0xff, 0xe1,
    ];
    let start = find(&code.bytes, &sequence).expect("the jump table sequence") + sequence.len();
    let entries: Vec<usize> = (0..4)
        .map(|k| {
            let at = start + 4 * k;
            let rel = i32::from_le_bytes(code.bytes[at..at + 4].try_into().unwrap());
            (start as i64 + i64::from(rel)) as usize
        })
        .collect();
    for (k, &entry) in entries.iter().enumerate() {
        // Each target is a `return`: its first restore, `mov rbx, [rsp + ...]`.
        assert_eq!(&code.bytes[entry..entry + 2], &[0x48, 0x8b], "entry {k}");
        assert!(entries[..k].iter().all(|it| *it != entry), "entry {k}");
    }
    // The default, edge 0, is the last entry: the first block after the table's.
    assert!(entries[3] < entries[0]);
}

#[test]
fn landing_pads_store_the_payload() {
    let func = build(sig(CallConv::SystemV, &[I64], &[I64]), |b, p| {
        let callee = import(b, 0, sig(CallConv::SystemV, &[I64], &[I64]), true);
        let returned = b.create_block();
        b.append_block_param(returned, I64);
        let landing = b.create_block();
        b.append_block_param(landing, I64);
        let normal = b.func.dfg.block_call(returned, &[ir::BlockArg::TryCallRet(0)]);
        let to = b.func.dfg.block_call(landing, &[ir::BlockArg::TryCallExn(0)]);
        let signature = b.func.dfg.ext_funcs[callee].signature;
        let table = b.func.dfg.exception_tables.push(ir::ExceptionTableData::new(
            signature,
            normal,
            [ir::ExceptionTableItem::Default(to)],
        ));
        b.ins().try_call(callee, &[p[0]], table);
        b.switch_to_block(returned);
        let r = b.block_params(returned)[0];
        b.ins().return_(&[r]);
        b.switch_to_block(landing);
        let e = b.block_params(landing)[0];
        b.ins().return_(&[e]);
    });
    let code = ours(&func, &LINUX);
    assert_eq!(code.calls.len(), 1);
    let [Handler::Default { landing }] = code.handlers[..] else { panic!("{:?}", code.handlers) };
    // `mov [r12 + disp], rax`.
    assert_eq!(&code.bytes[landing as usize..landing as usize + 2], &[0x49, 0x89]);
}

/// The immediate of the `add rsp` before the last `mov rsp, rbp`, `pop rbp`, `ret` (0 when
/// there is none): what the epilogue frees, the distance from `rbp` down to `rsp`.
fn freed(bytes: &[u8]) -> u32 {
    let end = bytes
        .windows(5)
        .rposition(|w| w == [0x48, 0x89, 0xec, 0x5d, 0xc3])
        .expect("an epilogue");
    if end >= 4 && bytes[end - 4..end - 1] == [0x48, 0x83, 0xc4] {
        bytes[end - 1] as i8 as u32
    } else if end >= 7 && bytes[end - 7..end - 4] == [0x48, 0x81, 0xc4] {
        u32::from_le_bytes([bytes[end - 4], bytes[end - 3], bytes[end - 2], bytes[end - 1]])
    } else {
        0
    }
}

/// A call with two of its eight arguments on the stack.
fn stack_arguments() -> ir::Function {
    build(sig(CallConv::SystemV, &[I64], &[]), |b, p| {
        let callee = import(b, 0, sig(CallConv::SystemV, &[I64; 8], &[]), true);
        b.ins().call(callee, &[p[0]; 8]);
        b.ins().return_(&[]);
    })
}

#[test]
fn the_frame_is_cranelifts_frame_to_fp_offset() {
    // Cranelift's `frame_to_fp_offset` (`fixed_frame_storage_size + clobber_size +
    // outgoing_args_size`) is what its epilogue frees, with saves, spills and stack arguments.
    for (func, target) in [
        (leaf_with_slot(64), LINUX),
        (leaf_with_slot(2000), LINUX),
        (pressure(CallConv::SystemV), LINUX),
        (pressure(CallConv::WindowsFastcall), WINDOWS),
        (stack_arguments(), LINUX),
    ] {
        let clif = cranelift(&func, &target);
        assert!(clif.frame > 0);
        assert_eq!(freed(&clif.bytes), clif.frame, "{:?}", target.os);
    }
    // Ours is the same distance in our frame.
    for (func, target) in [
        (leaf_with_slot(64), LINUX),
        (leaf_with_slot(2000), MACOS),
        (stack_arguments(), LINUX),
        (callers(CallConv::WindowsFastcall), WINDOWS),
    ] {
        let code = ours(&func, &target);
        assert_eq!(freed(&code.bytes), code.frame, "{:?}", target.os);
    }
    let frame = |func: &ir::Function| {
        X64::frame(func, &LINUX, Area { size: 1024, align: 16 })
            .unwrap_or_else(|e| panic!("{e}"))
    };
    let leaf = frame(&leaf_with_slot(64));
    let calls = frame(&stack_arguments());
    assert_eq!(X64::frame_size(&leaf), leaf.clobber + leaf.fixed);
    assert_eq!(X64::frame_size(&calls), X64::frame_size(&leaf) + 16);
}

#[test]
fn big_and_aligned_frames() {
    let func = build(sig(CallConv::SystemV, &[], &[]), |b, _| {
        b.create_sized_stack_slot(ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, 65536, 6));
        let callee = import(b, 0, sig(CallConv::SystemV, &[], &[]), true);
        b.ins().call(callee, &[]);
        b.ins().return_(&[]);
    });
    let code = ours(&func, &LINUX);
    // The probe loop, and `r12` rounded up to 64.
    assert!(find(&code.bytes, &[0x49, 0x89, 0xe3, 0x49, 0x81, 0xeb]).is_some());
    assert!(find(&code.bytes, &[0x49, 0x83, 0xe4, 0xc0]).is_some());
}

#[test]
fn refuses_what_cranelift_refuses() {
    let tail = build(sig(CallConv::SystemV, &[], &[]), |b, _| {
        let callee = import(b, 0, sig(CallConv::SystemV, &[], &[]), true);
        b.ins().return_call(callee, &[]);
    });
    let e = compile(&tail, &LINUX).unwrap_err();
    assert!(e.inst.is_some() && e.what.contains("tail"), "{e}");

    let catching = build(sig(CallConv::WindowsFastcall, &[], &[]), |b, _| {
        let callee = import(b, 0, sig(CallConv::WindowsFastcall, &[], &[]), true);
        let done = b.create_block();
        let normal = b.func.dfg.block_call(done, &[]);
        let signature = b.func.dfg.ext_funcs[callee].signature;
        let table = b.func.dfg.exception_tables.push(ir::ExceptionTableData::new(
            signature,
            normal,
            [ir::ExceptionTableItem::Default(normal)],
        ));
        b.ins().try_call(callee, &[], table);
        b.switch_to_block(done);
        b.ins().return_(&[]);
    });
    let e = compile(&catching, &WINDOWS).unwrap_err();
    assert!(e.what.contains("exceptions"), "{e}");

    let many = build(sig(CallConv::SystemV, &[], &[I64, I64, I64, I64]), |b, _| {
        let callee = import(b, 0, sig(CallConv::SystemV, &[], &[I64, I64, I64, I64]), true);
        let call = b.ins().call(callee, &[]);
        let rs = b.inst_results(call).to_vec();
        b.ins().return_(&rs);
    });
    let e = compile(&many, &LINUX).unwrap_err();
    assert!(e.what.contains("StructReturn"), "{e}");
}

fn places(sig: &ir::Signature) -> (Vec<Place>, u32) {
    let mut out = Vec::new();
    let size = abi::args(sig, |_, _, place| {
        out.push(place);
        Ok(())
    })
    .unwrap();
    (out, size)
}

fn returns(sig: &ir::Signature) -> Result<Vec<Place>, Error> {
    let mut out = Vec::new();
    abi::rets(sig, |_, _, place| {
        out.push(place);
        Ok(())
    })?;
    Ok(out)
}

#[test]
fn places_as_cranelifts_abi() {
    use Place::{Gpr, Implicit, Pair, Stack, Struct, Xmm};
    let sysv = CallConv::SystemV;
    let win = CallConv::WindowsFastcall;
    let regs = [RDI, RSI, RDX, RCX, R8, R9].map(Gpr);
    assert_eq!(places(&sig(sysv, &[I64; 7], &[])), ([&regs[..], &[Stack(0)]].concat(), 16));
    assert_eq!(
        places(&sig(sysv, &[F64; 9], &[])).0,
        [(0..8).map(Xmm).collect::<Vec<_>>(), vec![Stack(0)]].concat()
    );
    assert_eq!(
        places(&sig(sysv, &[I64, I128], &[])).0,
        [Gpr(RDI), Pair(Part::Gpr(RSI), Part::Gpr(RDX))]
    );
    assert_eq!(
        places(&sig(sysv, &[I64, I64, I64, I64, I64, I128, I64], &[])),
        ([&regs[..5], &[Pair(Part::Stack(0), Part::Stack(8)), Stack(16)]].concat(), 32)
    );
    let mut structs = sig(sysv, &[I64], &[]);
    structs.params.insert(0, AbiParam::special(I64, ArgumentPurpose::StructArgument(24)));
    assert_eq!(places(&structs), (vec![Struct { at: 0, size: 24 }, Gpr(RDI)], 32));

    assert_eq!(
        places(&sig(win, &[I64, F64, I64, F64, I64], &[])),
        (vec![Gpr(RCX), Xmm(1), Gpr(R8), Xmm(3), Stack(32)], 48)
    );
    assert_eq!(
        places(&sig(win, &[F32X4], &[])),
        (vec![Implicit { pointer: Part::Gpr(RCX), copy: 32 }], 48)
    );
    assert_eq!(
        places(&sig(win, &[I64, I64, I64, I64, I64, F32X4], &[])),
        (
            vec![
                Gpr(RCX),
                Gpr(RDX),
                Gpr(R8),
                Gpr(R9),
                Stack(32),
                Implicit { pointer: Part::Stack(40), copy: 48 }
            ],
            64
        )
    );
    assert_eq!(places(&sig(win, &[I128], &[])).0, [Pair(Part::Gpr(RCX), Part::Gpr(RDX))]);

    assert_eq!(returns(&sig(sysv, &[], &[I64, I64, I64])).unwrap(), [RAX, RDX, RCX].map(Gpr));
    assert!(returns(&sig(sysv, &[], &[I64; 4])).is_err());
    assert_eq!(returns(&sig(sysv, &[], &[F64, F32])).unwrap(), [Xmm(0), Xmm(1)]);
    assert_eq!(returns(&sig(sysv, &[], &[I128])).unwrap(), [Pair(Part::Gpr(RAX), Part::Gpr(RDX))]);
    assert_eq!(returns(&sig(win, &[], &[I64, I64])).unwrap(), [RAX, RDX].map(Gpr));
    assert!(returns(&sig(win, &[], &[I64; 3])).is_err());
    assert_eq!(returns(&sig(win, &[], &[F64, F64])).unwrap(), [Xmm(0), Xmm(1)]);
    assert!(returns(&sig(win, &[], &[F64, F64, F64])).is_err());
    assert_eq!(returns(&sig(win, &[], &[F32X4])).unwrap(), [Xmm(0)]);
}

#[test]
fn win_raw_is_cranelifts_encoding() {
    // A push, the frame pointer and two saves at known places, as Cranelift writes them.
    let rows = [
        (1, Rule::CfaOffset(16)),
        (1, Rule::Offset(6, -16)),
        (4, Rule::CfaRegister(6)),
        (12, Rule::Offset(3, -16 - 32)),
        (20, Rule::Offset(17 + 6, 16 - 16 - 32)),
    ];
    let mut raw = Vec::new();
    frame::win_raw(&rows, 32, &mut raw).unwrap();
    assert_eq!(raw, [1, 20, 6, 0x05, 20, 0x68, 1, 0, 12, 0x34, 0, 0, 4, 0x03, 1, 0x50]);
    let mut fixed = Vec::new();
    frame::with_saves(&raw, &mut fixed).unwrap();
    // One 8-byte save padded to 16, then the vector's 16: 32 bytes below `rbp`.
    assert_eq!(codes(&fixed), [(8, 6, 1), (4, 3, 0), (3, 0, 0), (2, 3, 0), (0, 5, 0)]);
    assert_eq!(fixed[3], 0x25);
}

#[test]
fn a_warm_compile_through_the_glue_allocates_only_into_recycled_vectors() {
    let library = mock();
    for target in [LINUX, WINDOWS] {
        let conv = conv(&target);
        let func = build(sig(conv, &[I64, F64], &[I64]), |b, p| {
            let callee = import(b, 0, sig(conv, &[I64, I64, F64], &[I64]), true);
            let k = b.ins().iconst(I64, 0x1234_5678_9abc);
            let call = b.ins().call(callee, &[p[0], k, p[1]]);
            let ret = b.inst_results(call)[0];
            b.ins().return_(&[ret]);
        });
        // What `compile_into` does, with the mock library.
        let into = |code: &mut Code| {
            compile_in::<X64, _>(&func, &target, &library, |it, _| code.copy_from(it)).unwrap();
        };
        let mut code = Code::default();
        into(&mut code);
        let ((), used) = blaze_alloc_count::measure(|| into(&mut code));
        assert_eq!(
            used,
            blaze_alloc_count::Used::default(),
            "a warm compile with the x86-64 glue for {:?}",
            target.os
        );
        assert!(!code.relocs.is_empty());
        assert_eq!(code, ours(&func, &target), "the reused code is the fresh one");
        match (&code.unwind, target.os) {
            (Some(Unwind::Windows { info, .. }), Os::Windows) => assert!(!info.is_empty()),
            (Some(Unwind::Dwarf { rows, .. }), Os::Linux) => assert!(!rows.is_empty()),
            (unwind, os) => panic!("{os:?}: {unwind:?}"),
        }
    }
}
