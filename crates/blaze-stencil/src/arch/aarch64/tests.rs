//! The AArch64 glue against Cranelift 0.137's own code of the same functions, with the flags
//! `hir-ty-codegen/target.rs` gives the dev ISA: the frame record, the probe, the unwind rows
//! and Windows codes, the relocations of calls, addresses and thread-locals, where stack
//! arguments go, the jump table sequence, and the places of arguments in AAPCS64 and Apple's
//! variant. The glue runs on a stand-in for the patcher that puts every value in a slot of
//! its own.

use std::sync::OnceLock;

use cranelift_codegen::{
    Context, RelocTarget,
    binemit::Reloc,
    control::ControlPlane,
    ir::{
        self, AbiParam, ArgumentPurpose, InstBuilder,
        immediates::Imm64,
        types::{F32, F64, I8, I32, I64, I128},
    },
    isa::{self, CallConv, OwnedTargetIsa, unwind::UnwindInfo},
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use super::{
    Aarch64, Ctx, Frame,
    abi::{self, Place, Reg},
    frame, glue, inst,
};
use crate::{
    Cpu, Error, Os, Target, TlsModel,
    arch::{Area, Glue, Loc},
    buf::{Buf, Label},
    code::{self, Code, Name, Rule, Unwind},
    enc::Form,
    patch::compile_in,
    shape::{self, Dst, Isa, Op, Shape, Src},
    table::format::{Hole, HoleKind, Reader, Variant, Writer},
};

/// Runs `f` on a compile's memory frame, as the patcher does.
fn on_frame<R>(f: impl FnOnce(&blaze_mem::Frame) -> R) -> R {
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, f)
}

pub(crate) const LINUX: Target = Target {
    arch: blaze_codegen::Arch::Aarch64,
    os: Os::Linux,
    cpu: Cpu::Baseline,
    pic: true,
    tls: TlsModel::ElfGd,
    probe_stack: true,
    probe_log2: 12,
};
const MACOS: Target = Target { os: Os::MacOs, cpu: Cpu::AppleM1, tls: TlsModel::MachO, ..LINUX };
const LINUX_STATIC: Target = Target { pic: false, ..LINUX };
const WINDOWS: Target = Target { os: Os::Windows, pic: false, tls: TlsModel::Coff, ..LINUX };

/// Value `v`'s slot is `SLOTS + 16 * v`; a call's results and a landing pad's payload have
/// their own. The area is small enough that no frame of the tests' is probed but
/// [`probes_are_cranelifts`]'.
const SLOTS: u32 = 64;
const RETURNED: u32 = 2048;
const PAYLOAD: u32 = 3072;
const AREA: Area = Area { size: 1024, align: 16 };

fn conv(target: &Target) -> CallConv {
    match target.os {
        Os::MacOs => CallConv::AppleAarch64,
        Os::Windows => CallConv::WindowsFastcall,
        Os::Linux => CallConv::SystemV,
    }
}

fn isa(target: &Target) -> OwnedTargetIsa {
    let triple = match target.os {
        Os::Linux => "aarch64-unknown-linux-gnu",
        Os::MacOs => "aarch64-apple-darwin",
        Os::Windows => "aarch64-pc-windows-msvc",
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
    if target.cpu == Cpu::AppleM1 {
        builder.enable("has_lse").expect("has_lse");
    }
    builder.finish(settings::Flags::new(flags)).expect("a cranelift ISA")
}

fn words(bytes: &[u8]) -> Vec<u32> {
    bytes.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect()
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
    words: Vec<u32>,
    relocs: Vec<code::Reloc>,
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
    let unwind = compiled.create_unwind_info(&*isa).expect("unwind information");
    let frame = compiled.buffer.frame_layout().expect("a frame layout").frame_to_fp_offset;
    Clif { words: words(compiled.code_buffer()), relocs, unwind, frame }
}

/// The patcher's stand-in.
struct Run<'f, 'm> {
    buf: Buf<'m>,
    func: &'f ir::Function,
    target: &'f Target,
    frame: &'f Frame,
    edges: [Label; 8],
    rows: Vec<(u32, Rule)>,
    sites: Vec<(u32, ir::Inst)>,
}

impl<'f, 'm> Run<'f, 'm> {
    fn new(
        func: &'f ir::Function,
        target: &'f Target,
        frame: &'f Frame,
        temp: &'m blaze_mem::Frame,
    ) -> Run<'f, 'm> {
        let mut buf = Buf::new_in(temp);
        let edges = std::array::from_fn(|_| buf.label());
        Run { buf, func, target, frame, edges, rows: Vec::new(), sites: Vec::new() }
    }
}

impl<'f, 'm> Ctx<'f, 'm> for Run<'f, 'm> {
    fn buf(&mut self) -> &mut Buf<'m> {
        &mut self.buf
    }

    fn func(&self) -> &'f ir::Function {
        self.func
    }

    fn target(&self) -> &'f Target {
        self.target
    }

    fn frame(&self) -> &'f Frame {
        self.frame
    }

    fn loc(&self, value: ir::Value) -> Loc {
        Loc::Slot(SLOTS + 16 * value.as_u32())
    }

    fn returned(&self, _: ir::Inst, k: usize) -> Loc {
        Loc::Slot(RETURNED + 16 * k as u32)
    }

    fn payload(&self, _: ir::Inst, k: usize) -> Loc {
        Loc::Slot(PAYLOAD + 16 * k as u32)
    }

    fn edge(&self, _: ir::Inst, i: usize) -> Label {
        self.edges[i]
    }

    fn rule_at(&mut self, at: u32, rule: Rule) {
        self.rows.push((at, rule));
    }

    fn call_site(&mut self, ret: u32, inst: ir::Inst) {
        self.sites.push((ret, inst));
    }

    fn name(&self, name: &ir::ExternalName) -> Result<Name, Error> {
        Ok(name_of(self.func, name))
    }
}

/// The glue's code of `func`: the prologue, then each instruction the glue takes (the
/// stencils' left out), then the edges' labels, one `nop` each.
struct Out {
    words: Vec<u32>,
    relocs: Vec<code::Reloc>,
    prologue: u32,
    /// Each glue instruction and the bytes it wrote.
    spans: Vec<(ir::Inst, u32, u32)>,
    edges: Vec<u32>,
    rows: Vec<(u32, Rule)>,
    sites: Vec<(u32, ir::Inst)>,
    frame: Frame,
}

impl Out {
    fn span(&self, opcode: ir::Opcode, func: &ir::Function) -> &[u32] {
        let &(_, start, end) = self
            .spans
            .iter()
            .find(|(inst, ..)| func.dfg.insts[*inst].opcode() == opcode)
            .unwrap_or_else(|| panic!("no {opcode}"));
        &self.words[start as usize / 4..end as usize / 4]
    }
}

fn whole(func: &ir::Function, target: &Target) -> Out {
    let frame = Aarch64::frame(func, target, AREA).unwrap_or_else(|e| panic!("{e}"));
    on_frame(|temp| whole_on(func, target, frame, temp))
}

fn whole_on(func: &ir::Function, target: &Target, frame: Frame, temp: &blaze_mem::Frame) -> Out {
    let mut run = Run::new(func, target, &frame, temp);
    glue::prologue(&mut run).unwrap_or_else(|e| panic!("{e}"));
    let prologue = run.buf.len();
    let mut spans = Vec::new();
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            if glue::takes(func.dfg.insts[inst].opcode()) {
                let start = run.buf.len();
                glue::inst(&mut run, inst).unwrap_or_else(|e| panic!("{e}"));
                spans.push((inst, start, run.buf.len()));
            }
        }
    }
    let mut edges = Vec::new();
    for label in run.edges {
        edges.push(run.buf.len());
        run.buf.bind(label);
        run.buf.put4(0xd503_201f);
    }
    run.buf.resolve().unwrap_or_else(|e| panic!("{e}"));
    run.buf.sort_relocs();
    let (words, relocs) = (words(run.buf.bytes()), run.buf.relocs().to_vec());
    let Run { rows, sites, .. } = run;
    Out { words, relocs, prologue, spans, edges, rows, sites, frame }
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

fn symbol(
    b: &mut FunctionBuilder<'_>,
    index: u32,
    offset: i64,
    colocated: bool,
    tls: bool,
) -> ir::GlobalValue {
    let name = b.func.declare_imported_user_function(ir::UserExternalName::new(1, index));
    b.create_global_value(ir::GlobalValueData::Symbol {
        name: ir::ExternalName::user(name),
        offset: Imm64::new(offset),
        colocated,
        tls,
    })
}

fn leaf(conv: CallConv) -> ir::Function {
    build(sig(conv, &[], &[]), |b, _| {
        b.ins().return_(&[]);
    })
}

/// A colocated call of ten `ty` arguments (two on the stack), a call through the GOT or an
/// absolute address, one through a `func_addr`, and a data symbol's address at an offset.
fn callers(conv: CallConv, ty: ir::Type) -> ir::Function {
    build(sig(conv, &[I64], &[I64]), |b, params| {
        let near = import(b, 1, sig(conv, &[ty; 10], &[I64]), true);
        let far = import(b, 2, sig(conv, &[], &[]), false);
        let arg = if ty == I64 { params[0] } else { b.ins().ireduce(ty, params[0]) };
        let call = b.ins().call(near, &[arg; 10]);
        let result = b.inst_results(call)[0];
        b.ins().call(far, &[]);
        let addr = b.ins().func_addr(I64, far);
        let indirect = b.import_signature(sig(conv, &[], &[]));
        b.ins().call_indirect(indirect, addr, &[]);
        let data = symbol(b, 3, 16, true, false);
        let data = b.ins().symbol_value(I64, data);
        let sum = b.ins().iadd(result, data);
        b.ins().return_(&[sum]);
    })
}

fn switch() -> ir::Function {
    build(sig(CallConv::SystemV, &[I32], &[I32]), |b, params| {
        let blocks: Vec<ir::Block> = (0..4).map(|_| b.create_block()).collect();
        let default = b.func.dfg.block_call(blocks[0], &[]);
        let targets: Vec<ir::BlockCall> =
            blocks[1..].iter().map(|block| b.func.dfg.block_call(*block, &[])).collect();
        let table = b.create_jump_table(ir::JumpTableData::new(default, &targets));
        b.ins().br_table(params[0], table);
        for (i, block) in blocks.into_iter().enumerate() {
            b.switch_to_block(block);
            let v = b.ins().iconst(I32, i as i64);
            b.ins().return_(&[v]);
        }
    })
}

fn thread_local(conv: CallConv) -> ir::Function {
    build(sig(conv, &[], &[I64]), |b, _| {
        let gv = symbol(b, 4, 0, false, true);
        let v = b.ins().tls_value(I64, gv);
        b.ins().return_(&[v]);
    })
}

/// `symbol_value` of a data symbol (16 bytes in, not colocated) and `tls_value` of a
/// thread-local.
fn globals(conv: CallConv) -> ir::Function {
    build(sig(conv, &[], &[I64]), |b, _| {
        let data = symbol(b, 3, 16, false, false);
        let v = b.ins().symbol_value(I64, data);
        let tls = symbol(b, 4, 0, false, true);
        let t = b.ins().tls_value(I64, tls);
        let sum = b.ins().iadd(v, t);
        b.ins().return_(&[sum]);
    })
}

/// The relocations but the tier counters', which Cranelift's code has none of.
fn kinds(relocs: &[code::Reloc]) -> Vec<(Reloc, Name, i64)> {
    relocs
        .iter()
        .filter(|reloc| reloc.name != Name::Counter)
        .map(|reloc| (reloc.kind, reloc.name, reloc.addend))
        .collect()
}

/// `(log2 of the size, offset)` of each integer store at `[sp, #off]`, either form.
fn sp_stores(words: &[u32]) -> Vec<(u32, u32)> {
    words
        .iter()
        .filter(|w| (*w >> 5) & 31 == 31)
        .filter_map(|&w| {
            let size = w >> 30;
            if w & 0x3fe0_0c00 == 0x3800_0000 {
                Some((size, (w >> 12) & 0x1ff))
            } else if w & 0x3fc0_0000 == 0x3900_0000 {
                Some((size, ((w >> 10) & 0xfff) << size))
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn frame_record_epilogue_and_rows_are_cranelifts() {
    for target in [LINUX, MACOS] {
        let func = leaf(conv(&target));
        let clif = cranelift(&func, &target);
        let ours = whole(&func, &target);
        // `stp x29, x30, [sp, #-16]!`, `mov x29, sp`.
        assert_eq!(ours.words[..2], [0xa9bf_7bfd, 0x9100_03fd]);
        assert_eq!(ours.words[..2], clif.words[..2], "{:?}", target.os);
        // `ldp x29, x30, [sp], #16`, `ret`.
        let ret = ours.span(ir::Opcode::Return, &func);
        assert_eq!(ret[ret.len() - 2..], [0xa8c1_7bfd, 0xd65f_03c0]);
        assert_eq!(ret[ret.len() - 2..], clif.words[clif.words.len() - 2..]);
        // Apple's unsigned return addresses, the record's rows and the CFA moved to `x29`,
        // as Cranelift's (its leaf saves nothing, so these are all of its rows).
        let apple = usize::from(target.os == Os::MacOs);
        let rows: Vec<String> = ours.rows[..4 + apple]
            .iter()
            .map(|row| match row {
                (at, Rule::RaSignState(signed)) => {
                    format!("({at}, Aarch64SetPointerAuth {{ return_addresses: {signed} }})")
                }
                row => format!("{row:?}"),
            })
            .collect();
        assert_eq!(rows[3 + apple], "(8, CfaRegister(29))");
        let theirs = format!("{:?}", clif.unwind);
        assert!(theirs.contains(&format!("[{}]", rows.join(", "))), "{theirs} is not {rows:?}");
    }
}

#[test]
fn saves_every_callee_saved_register() {
    let func = leaf(CallConv::SystemV);
    let ours = whole(&func, &LINUX);
    let saves: Vec<u32> = frame::X_PAIRS
        .iter()
        .map(|&(a, b)| inst::stp_pre(a, b, inst::SP, -16))
        .chain(frame::D_PAIRS.iter().map(|&(a, b)| inst::stp_d_pre(a, b, inst::SP, -16)))
        .collect();
    assert_eq!(ours.words[2..11], saves[..]);
    // `stp x27, x28, [sp, #-16]!` first, `stp d8, d9, [sp, #-16]!` last.
    assert_eq!(saves[0], 0xa9bf_73fb);
    assert_eq!(saves[8], 0x6dbf_27e8);
    // A row for each, from the CFA: the record at -16, then down by pairs.
    assert!(!ours.rows.iter().any(|(_, rule)| matches!(rule, Rule::RaSignState(_))));
    assert_eq!(ours.rows.len(), 3 + 1 + 18);
    for (reg, at) in [(28, -24), (27, -32), (20, -88), (19, -96), (79, -104), (72, -160)] {
        assert!(ours.rows.iter().any(|(_, rule)| *rule == Rule::Offset(reg, at)), "{reg} at {at}");
    }
    let ret = ours.span(ir::Opcode::Return, &func);
    let restores: Vec<u32> = frame::D_PAIRS
        .iter()
        .rev()
        .map(|&(a, b)| inst::ldp_d_post(a, b, inst::SP, 16))
        .chain(frame::X_PAIRS.iter().rev().map(|&(a, b)| inst::ldp_post(a, b, inst::SP, 16)))
        .collect();
    assert_eq!(ret[ret.len() - 11..ret.len() - 2], restores[..]);
}

#[test]
fn realigned_frames_round_x20_up() {
    let func = leaf(CallConv::SystemV);
    let frame = Aarch64::frame(&func, &LINUX, Area { size: 1024, align: 64 })
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(frame.stack, 1024 + 48);
    let base = on_frame(|temp| {
        let mut buf = Buf::new_in(temp);
        frame::set_base(&mut buf, &frame);
        words(buf.bytes())
    });
    // `add x20, sp, #63`, `and x20, x20, #-64`.
    assert_eq!(base, [0x9100_fff4, 0x927a_e694]);
    assert_eq!(Aarch64::slot_base(&frame), (20, 0));
    let frame = Aarch64::frame(&func, &LINUX, AREA).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(Aarch64::slot_base(&frame), (29, -(1024 + frame::SAVES as i32)));
}

#[test]
fn probes_are_cranelifts() {
    // Three pages unrolled (`sub sp`, `stur wzr, [sp]` each, then `add sp`), sixteen in a loop.
    for (size, len) in [(3 * 4096, 7), (16 * 4096, 6)] {
        let func = build(sig(CallConv::SystemV, &[], &[]), |b, _| {
            b.create_sized_stack_slot(ir::StackSlotData::new(
                ir::StackSlotKind::ExplicitSlot,
                size,
                4,
            ));
            b.ins().return_(&[]);
        });
        let clif = cranelift(&func, &LINUX);
        // The same bytes below the frame record: the saves and an area of the rest.
        let frame = Aarch64::frame(
            &leaf(CallConv::SystemV),
            &LINUX,
            Area { size: size - frame::SAVES, align: 16 },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let ours = on_frame(|temp| {
            let mut buf = Buf::new_in(temp);
            frame::enter(&mut buf, &frame, &mut frame::Rows::default());
            words(buf.bytes())
        });
        assert_eq!(ours[2..2 + len], clif.words[2..2 + len], "{size}");
    }
}

/// What the epilogue frees before the last `ldp x29, x30, [sp], #16` and `ret`: its `add sp`
/// immediates and its restores' post-index steps, the distance from `x29` down to `sp`.
fn freed(words: &[u32]) -> u32 {
    let end = words
        .windows(2)
        .rposition(|w| w == [0xa8c1_7bfd, inst::RET])
        .expect("an epilogue");
    let mut bytes = 0;
    for &w in words[..end].iter().rev() {
        if w & 0xffc0_03e0 == 0xa8c0_03e0 || w & 0xffc0_03e0 == 0x6cc0_03e0 {
            // `ldp` of X or D registers from `[sp], #imm`.
            bytes += ((w >> 15) & 0x7f) * 8;
        } else if w & 0xffe0_0fe0 == 0xf840_07e0 || w & 0xffe0_0fe0 == 0xfc40_07e0 {
            // `ldr` of one X or D register from `[sp], #imm`.
            bytes += (w >> 12) & 0x1ff;
        } else if w & 0xff80_03ff == 0x9100_03ff {
            // `add sp, sp, #imm`, shifted by 12 or not.
            bytes += ((w >> 10) & 0xfff) << if w & (1 << 22) != 0 { 12 } else { 0 };
        } else {
            break;
        }
    }
    bytes
}

/// A leaf whose frame is one stack slot of `bytes`.
fn leaf_with_slot(conv: CallConv, bytes: u32) -> ir::Function {
    build(sig(conv, &[], &[]), |b, _| {
        b.create_sized_stack_slot(ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, bytes, 4));
        b.ins().return_(&[]);
    })
}

#[test]
fn the_frame_is_cranelifts_frame_to_fp_offset() {
    // Cranelift's `frame_to_fp_offset` (`fixed_frame_storage_size + clobber_size +
    // outgoing_args_size`) is what its epilogue frees, with saves and stack arguments.
    for (func, target) in [
        (leaf_with_slot(CallConv::SystemV, 64), LINUX),
        (leaf_with_slot(CallConv::AppleAarch64, 2000), MACOS),
        (callers(CallConv::SystemV, I64), LINUX),
        (callers(CallConv::AppleAarch64, I32), MACOS),
    ] {
        let clif = cranelift(&func, &target);
        assert!(clif.frame > 0);
        assert_eq!(freed(&clif.words), clif.frame, "{:?}", target.os);
    }
    // Ours is the same distance in our frame, and the compile's `Code` says it.
    for (func, target) in [
        (leaf(CallConv::SystemV), LINUX),
        (callers(CallConv::SystemV, I64), LINUX),
        (callers(CallConv::AppleAarch64, I32), MACOS),
    ] {
        let ours = whole(&func, &target);
        let size = Aarch64::frame_size(&ours.frame);
        assert_eq!(freed(&ours.words), size, "{:?}", target.os);
        assert_eq!(size, frame::SAVES + AREA.size + ours.frame.outgoing);
    }
    let library = mock();
    for target in [LINUX, MACOS, WINDOWS] {
        let func = leaf_with_slot(conv(&target), 64);
        let code = compile_in::<Aarch64, _>(&func, &target, &library, |it, _| it.to_code())
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(freed(&words(&code.bytes)), code.frame, "{:?}", target.os);
    }
}

#[test]
fn windows_codes_are_in_cranelifts_order() {
    let func = leaf(CallConv::WindowsFastcall);
    let clif = cranelift(&func, &WINDOWS);
    let Some(UnwindInfo::WindowsArm64(info)) = &clif.unwind else {
        panic!("{:?}", clif.unwind);
    };
    let mut theirs = vec![0; usize::from(info.code_words()) * 4];
    info.emit(&mut theirs);
    // A leaf of Cranelift's saves nothing: `set_fp`, `save_fplr_x`.
    assert_eq!(theirs, [0xe1, 0x81, 0, 0]);
    let frame = Aarch64::frame(&func, &WINDOWS, Area { size: 0, align: 16 })
        .unwrap_or_else(|e| panic!("{e}"));
    let mut codes = Vec::new();
    for a in [8u16, 10, 12, 14] {
        codes.extend_from_slice(&(0xda00 | (a - 8) << 6 | 1).to_be_bytes());
    }
    for a in [19u16, 21, 23, 25, 27] {
        codes.extend_from_slice(&(0xcc00 | (a - 19) << 6 | 1).to_be_bytes());
    }
    codes.extend_from_slice(&theirs[..2]);
    codes.resize(codes.len().div_ceil(4) * 4, 0);
    let mut written = Vec::new();
    frame::windows_codes(&frame, &mut written);
    assert_eq!(written, codes);
}

#[test]
fn calls_and_addresses_relocate_as_cranelifts() {
    for target in [LINUX, MACOS, LINUX_STATIC] {
        let func = callers(conv(&target), I64);
        let clif = cranelift(&func, &target);
        let ours = whole(&func, &target);
        assert_eq!(kinds(&ours.relocs), kinds(&clif.relocs), "{:?} pic {}", target.os, target.pic);
        // One site per call, each right after its `bl`/`blr`.
        assert_eq!(ours.sites.len(), 3);
        for (ret, _) in &ours.sites {
            let call = ours.words[*ret as usize / 4 - 1];
            assert!(call == inst::BL || call & 0xffff_fc1f == 0xd63f_0000, "{call:#x}");
        }
    }
    // The near call is `bl` at its `Arm64Call`.
    let ours = whole(&callers(CallConv::SystemV, I64), &LINUX);
    let bl = ours.relocs.iter().find(|reloc| reloc.kind == Reloc::Arm64Call).expect("a bl");
    assert_eq!(ours.words[bl.at as usize / 4], 0x9400_0000);
}

#[test]
fn stack_arguments_are_where_cranelift_puts_them() {
    // AAPCS64 gives each stack argument 8 bytes; Apple's packs them at their size.
    for (target, ty, expect) in [
        (LINUX, I64, [(3, 0), (3, 8)]),
        (LINUX, I32, [(2, 0), (2, 8)]),
        (MACOS, I32, [(2, 0), (2, 4)]),
    ] {
        let func = callers(conv(&target), ty);
        let clif = cranelift(&func, &target);
        let ours = whole(&func, &target);
        assert_eq!(ours.frame.outgoing, 16);
        assert_eq!(sp_stores(ours.span(ir::Opcode::Call, &func)), expect);
        let theirs = sp_stores(&clif.words);
        for store in expect {
            assert!(theirs.contains(&store), "{:?} {ty}: {store:?} not in {theirs:?}", target.os);
        }
    }
}

#[test]
fn apple_extends_narrow_arguments_and_aapcs64_does_not() {
    for (target, load) in [(MACOS, 0x3880_0000), (LINUX, 0x3840_0000)] {
        let mut callee = sig(conv(&target), &[], &[]);
        callee.params.push(AbiParam::new(I8).sext());
        let func = build(sig(conv(&target), &[I8], &[]), |b, params| {
            let f = import(b, 1, callee, true);
            b.ins().call(f, &[params[0]]);
            b.ins().return_(&[]);
        });
        let ours = whole(&func, &target);
        // `ldursb x0, [x20, #off]` on Apple's, `ldurb w0` on AAPCS64's.
        assert_eq!(ours.span(ir::Opcode::Call, &func)[0] & 0xffe0_0c1f, load, "{:?}", target.os);
    }
}

#[test]
fn br_table_is_cranelifts_sequence() {
    let func = switch();
    let clif = cranelift(&func, &LINUX);
    let ours = whole(&func, &LINUX);
    // Cranelift 0.137 branches to no default: `csel xN, xIndex, xM, lo` clamps an index out
    // of bounds to the default's entry, the table's last.
    let is_csel_lo = |w: &u32| *w & 0xffe0_fc00 == 0x9a80_3000;
    let theirs = clif.words.iter().position(is_csel_lo).expect("Cranelift's csel");
    let table = ours.span(ir::Opcode::BrTable, &func);
    let at = table.iter().position(is_csel_lo).expect("csel");
    // `cmp wN, #3` and `movz xM, #3` before; `adr #16`, `ldrsw [.., uxtw #2]`, `add`, `br`
    // after, as Cranelift's but for the registers.
    assert_eq!(table[at - 2] & 0xffff_fc1f, clif.words[theirs - 2] & 0xffff_fc1f);
    assert_eq!(table[at - 1] & 0xffff_ffe0, clif.words[theirs - 1] & 0xffff_ffe0);
    let masks = [0xffff_ffe0, 0xffe0_fc00, 0xffe0_fc00, 0xffff_fc1f];
    for (k, mask) in masks.into_iter().enumerate() {
        assert_eq!(table[at + 1 + k] & mask, clif.words[theirs + 1 + k] & mask, "{k}");
    }
    // Four entries, each its target's offset from the table: the three, then the default.
    assert_eq!(table.len(), at + 5 + 4);
    let (_, start, _) = ours
        .spans
        .iter()
        .find(|(inst, ..)| func.dfg.insts[*inst].opcode() == ir::Opcode::BrTable)
        .copied()
        .expect("br_table");
    let base = start + 4 * (at as u32 + 5);
    for (i, edge) in [1, 2, 3, 0].into_iter().enumerate() {
        assert_eq!(table[at + 5 + i] as i32, ours.edges[edge] as i32 - base as i32);
    }
}

#[test]
fn thread_locals_are_cranelifts() {
    for target in [LINUX, MACOS] {
        let func = thread_local(conv(&target));
        let clif = cranelift(&func, &target);
        let ours = whole(&func, &target);
        assert_eq!(kinds(&ours.relocs), kinds(&clif.relocs), "{:?}", target.os);
        let at = |relocs: &[code::Reloc]| {
            let at: Vec<u32> =
                relocs.iter().filter(|r| r.name != Name::Counter).map(|r| r.at).collect();
            at.iter().map(|it| it - at[0]).collect::<Vec<u32>>()
        };
        assert_eq!(at(&ours.relocs), at(&clif.relocs));
        assert_eq!(ours.sites.len(), 1);
        if target.tls == TlsModel::MachO {
            // `adrp x0`, `ldr x0, [x0]`, `ldr x1, [x0]`, `blr x1`: Cranelift's fixed registers.
            let first = |relocs: &[code::Reloc]| {
                relocs.iter().find(|r| r.name != Name::Counter).expect("a relocation").at as usize
                    / 4
            };
            let ours_at = first(&ours.relocs);
            let theirs_at = first(&clif.relocs);
            assert_eq!(
                ours.words[ours_at..ours_at + 4],
                [0x9000_0000, 0xf940_0000, 0xf940_0001, 0xd63f_0020]
            );
            assert_eq!(ours.words[ours_at..ours_at + 4], clif.words[theirs_at..theirs_at + 4]);
        }
    }
}

#[test]
fn global_values_are_cranelifts() {
    for target in [LINUX, MACOS, LINUX_STATIC] {
        let func = globals(conv(&target));
        let clif = cranelift(&func, &target);
        let ours = whole(&func, &target);
        assert_eq!(kinds(&ours.relocs), kinds(&clif.relocs), "{:?} pic {}", target.os, target.pic);
        // The thread-local's is a call.
        assert_eq!(ours.sites.len(), 1);
    }
    // Static and not colocated: `ldr x16, #8`, `b #12`, the literal at `Abs8`.
    let func = globals(CallConv::SystemV);
    let ours = whole(&func, &LINUX_STATIC);
    let span = ours.span(ir::Opcode::SymbolValue, &func);
    assert_eq!(span[..2], [inst::ldr_lit(inst::X16, 8), inst::b(3)]);
    let literal = ours.relocs.iter().find(|reloc| reloc.kind == Reloc::Abs8).expect("an Abs8");
    assert_eq!((literal.name, literal.addend), (Name::User { namespace: 1, index: 3 }, 16));
}

#[test]
fn bounds_hold() {
    for target in [LINUX, MACOS, LINUX_STATIC] {
        let conv = conv(&target);
        for func in [
            leaf(conv),
            callers(conv, I64),
            callers(conv, I32),
            thread_local(conv),
            globals(conv),
            switch(),
        ] {
            let ours = whole(&func, &target);
            assert!(ours.prologue <= ours.frame.entry);
            assert!(ours.frame.entry <= Aarch64::bound_fixed(&ours.frame));
            for &(inst, start, end) in &ours.spans {
                assert!(
                    end - start <= Aarch64::bound(&func, &target, inst),
                    "{}",
                    func.dfg.insts[inst].opcode()
                );
            }
        }
    }
}

#[test]
fn never_writes_x18() {
    for target in [LINUX, MACOS, LINUX_STATIC] {
        let conv = conv(&target);
        for func in [leaf(conv), callers(conv, I64), callers(conv, I32), thread_local(conv)] {
            let ours = whole(&func, &target);
            assert!(ours.words.iter().all(|w| w & 31 != 18), "{:?}", target.os);
        }
    }
}

#[test]
fn relays_and_counters_use_only_x16_and_x17() {
    let (relays, words, relocs) = on_frame(|temp| {
        let mut buf = Buf::new_in(temp);
        glue::relay(&mut buf, 40_000, 80_016, 16);
        glue::relay(&mut buf, 40_000, 80_000, 8);
        let relays = buf.len();
        glue::count(&mut buf, glue::BACK_EDGES);
        buf.sort_relocs();
        (relays, words(buf.bytes()), buf.relocs().to_vec())
    });
    // Each instruction's first register (what it writes, or a store's data) is a scratch one.
    assert!(words.iter().all(|w| matches!(w & 31, 16 | 17)), "{words:x?}");
    assert_eq!(
        relocs,
        [
            code::Reloc {
                at: relays,
                kind: Reloc::Aarch64AdrPrelPgHi21,
                name: Name::Counter,
                addend: 8
            },
            code::Reloc {
                at: relays + 4,
                kind: Reloc::Aarch64AddAbsLo12Nc,
                name: Name::Counter,
                addend: 8
            },
        ]
    );
    // `ldr x17, [x16]`, `add x17, x17, #1`, `str x17, [x16]`.
    assert_eq!(words[words.len() - 3..], [0xf940_0211, 0x9100_0631, 0xf900_0211]);
}

#[test]
fn landing_pads_store_the_payload() {
    let func = leaf(CallConv::SystemV);
    let frame = Aarch64::frame(&func, &LINUX, AREA).unwrap_or_else(|e| panic!("{e}"));
    let inst =
        func.layout.first_inst(func.layout.entry_block().expect("a block")).expect("an inst");
    let written = on_frame(|temp| {
        let mut run = Run::new(&func, &LINUX, &frame, temp);
        glue::landing(&mut run, inst).unwrap_or_else(|e| panic!("{e}"));
        words(run.buf.bytes())
    });
    // `str x0, [x20, #3072]`, `str x1, [x20, #3088]`.
    assert_eq!(written, [0xf906_0280, 0xf906_0a81]);
}

#[test]
fn tail_calls_outside_tail_and_other_conventions_are_errors() {
    let func = build(sig(CallConv::SystemV, &[], &[]), |b, _| {
        let f = import(b, 1, sig(CallConv::Tail, &[], &[]), true);
        b.ins().return_call(f, &[]);
    });
    let inst =
        func.layout.first_inst(func.layout.entry_block().expect("a block")).expect("an inst");
    let error = Aarch64::frame(&func, &LINUX, AREA).map(drop).map_err(|e| e.inst);
    assert_eq!(error, Err(Some(inst)), "a `system_v` function's return call is refused");
    assert!(Aarch64::frame(&leaf(CallConv::Tail), &LINUX, AREA).is_ok());
    assert!(Aarch64::frame(&leaf(CallConv::Winch), &LINUX, AREA).is_err());
}

#[test]
fn a_return_call_of_more_stack_arguments_grows_the_frame_as_cranelift_does() {
    // Eight `i64`s from `x2`: two on the stack, 16 bytes the function was not given.
    let func = build(sig(CallConv::Tail, &[], &[]), |b, _| {
        let f = import(b, 1, sig(CallConv::Tail, &[I64; 8], &[]), true);
        let args: Vec<_> = (0..8).map(|i| b.ins().iconst(I64, i)).collect();
        b.ins().return_call(f, &args);
    });
    let out = whole(&func, &LINUX);
    assert_eq!((out.frame.pop, out.frame.grow), (16, 16));
    // `sub sp, sp, #16`, `ldur x29, [sp, #16]`, `stp x29, x30, [sp]`, `mov x29, sp`.
    let grow = [0xd100_43ff, 0xf841_03fd, 0xa900_7bfd, 0x9100_03fd];
    let has = |words: &[u32]| words.windows(4).any(|w| w == grow);
    assert!(has(&out.words[..out.prologue as usize / 4]), "the glue's prologue grows");
    assert!(has(&cranelift(&func, &LINUX).words), "Cranelift's prologue grows the same way");
    // The new arguments fill the area, so no `add sp` before the colocated `b`.
    let call = out.span(ir::Opcode::ReturnCall, &func);
    assert_eq!(call[call.len() - 1], 0x1400_0000);
    let reloc = out.relocs.last().expect("the callee's relocation");
    assert_eq!((reloc.kind, reloc.at / 4), (Reloc::Arm64Call, (out.words.len() - 8 - 1) as u32));
}

#[test]
fn places_are_aapcs64s_and_apples() {
    let places = |conv: CallConv, params: &[ir::Type]| {
        let mut out = Vec::new();
        let size = abi::args(&sig(conv, params, &[]), |_, _, place| {
            out.push(place);
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}"));
        (out, size)
    };
    // An `i128` after an `i64`: AAPCS64 starts it at an even register, Apple's does not.
    assert_eq!(places(CallConv::SystemV, &[I64, I128]).0[1], Place::Pair(2, 3));
    assert_eq!(places(CallConv::AppleAarch64, &[I64, I128]).0[1], Place::Pair(1, 2));
    // Ten `i8`s: two on the stack, 8 bytes each in AAPCS64, 1 in Apple's; 16 bytes either way.
    let (sysv, size) = places(CallConv::SystemV, &[I8; 10]);
    assert_eq!((sysv[8..].to_vec(), size), (vec![Place::Stack(0, I8), Place::Stack(8, I8)], 16));
    let (apple, size) = places(CallConv::AppleAarch64, &[I8; 10]);
    assert_eq!((apple[8..].to_vec(), size), (vec![Place::Stack(0, I8), Place::Stack(1, I8)], 16));
    // Floats count in `v0`..`v7`, apart from the integers.
    assert_eq!(
        places(CallConv::SystemV, &[F32, I64, F32]).0,
        [Place::Reg(Reg::V(0), F32), Place::Reg(Reg::X(0), I64), Place::Reg(Reg::V(1), F32)]
    );
    // The StructReturn pointer in `x8`, beside the others.
    let mut sret = sig(CallConv::SystemV, &[I64], &[]);
    sret.params.insert(0, AbiParam::special(I64, ArgumentPurpose::StructReturn));
    let mut got = Vec::new();
    abi::args(&sret, |_, _, place| {
        got.push(place);
        Ok(())
    })
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(got, [Place::Reg(Reg::X(8), I64), Place::Reg(Reg::X(0), I64)]);
    // Nine `i64` returns do not fit: Cranelift asks for a StructReturn argument.
    assert!(abi::rets(&sig(CallConv::SystemV, &[], &[I64; 9]), |_, _, _| Ok(())).is_err());
}

// A mock library for the patcher on AArch64, as x86-64's tests make one: each stencil a word
// naming its shape, one word per hole and a tail `b`. Only what the glue does not write needs
// one.

const OPS: [Op; 4] = [Op::Copy, Op::Iconst, Op::Fconst, Op::Jump];

/// `movz x16, #0`, then `movk x16, #0, lsl 16`, `lsl 32` and `lsl 48`.
const MOVS: [u32; 4] = [0xd280_0010, 0xf2a0_0010, 0xf2c0_0010, 0xf2e0_0010];
/// `ldr x16, [x20]` and `str x16, [x20]`, their imm12 the hole.
const LDR: u32 = 0xf940_0290;
const STR: u32 = 0xf900_0290;
/// `b .`, its imm26 the hole.
const B: u32 = 0x1400_0000;

/// Each hole of `shape`'s mock stencil, by the instruction word it is in.
fn fields(shape: Shape) -> Vec<(HoleKind, Form, u32)> {
    let info = shape.op.info();
    let mut out = Vec::new();
    match shape.op {
        Op::Iconst | Op::Fconst => {
            let movs = if shape.aux == 1 { &MOVS[..] } else { &MOVS[..2] };
            let forms = [Form::A64MovwG0Nc, Form::A64MovwG1Nc, Form::A64MovwG2Nc, Form::A64MovwG3];
            out.extend(movs.iter().zip(forms).map(|(&word, form)| (HoleKind::Imm0, form, word)));
        }
        _ => {
            for (n, src) in shape.srcs.into_iter().enumerate() {
                if src == Src::Slot {
                    out.push(([HoleKind::Slot0, HoleKind::Slot1, HoleKind::Slot2][n], Form::A64Ldst64Lo12, LDR));
                }
            }
        }
    }
    if matches!(shape.dst, Dst::Slot | Dst::Both) {
        out.push((HoleKind::Result0, Form::A64Ldst64Lo12, STR));
    }
    if info.branches {
        out.push((HoleKind::Target, Form::A64Jump26, B));
    }
    out
}

/// A word naming the shape, its holes' words, and a `b` to the next stencil: the tail.
fn stencil(shape: Shape) -> (Vec<u8>, Vec<Hole>) {
    let mut code = shape.key().to_le_bytes().to_vec();
    let mut holes = Vec::new();
    for (kind, form, word) in fields(shape).into_iter().chain([(HoleKind::Next, Form::A64Jump26, B)]) {
        holes.push(Hole { at: code.len() as u16, form, kind, addend: 0 });
        code.extend_from_slice(&word.to_le_bytes());
    }
    (code, holes)
}

fn mock() -> Reader<'static> {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    Reader::new(BYTES.get_or_init(|| library_of(&OPS))).unwrap()
}

/// A library of the mock stencils of every AArch64 shape of `ops`.
pub(crate) fn library_of(ops: &[Op]) -> Vec<u8> {
    let mut writer = Writer::new(Variant::Aarch64);
    shape::each_on(Isa::Aarch64, |shape| {
        if ops.contains(&shape.op) {
            let (code, holes) = stencil(shape);
            writer.push(shape, &code, 4, &holes).unwrap();
        }
    });
    writer.finish(|_| [0; 32]).unwrap()
}

#[test]
fn a_warm_compile_through_the_glue_into_a_reused_code_allocates_nothing() {
    let library = mock();
    for target in [LINUX, MACOS, WINDOWS] {
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
            compile_in::<Aarch64, _>(&func, &target, &library, |it, _| code.copy_from(it))
                .unwrap_or_else(|e| panic!("{e}"));
        };
        let mut code = Code::default();
        into(&mut code);
        let ((), used) = blaze_alloc_count::measure(|| into(&mut code));
        assert_eq!(
            used,
            blaze_alloc_count::Used::default(),
            "a warm compile with the AArch64 glue for {:?}",
            target.os
        );
        assert!(!code.relocs.is_empty());
        let fresh = compile_in::<Aarch64, _>(&func, &target, &library, |it, _| it.to_code()).unwrap();
        assert_eq!(code, fresh, "the reused code is the fresh one");
        match (&code.unwind, target.os) {
            (Some(Unwind::WindowsArm64 { codes, .. }), Os::Windows) => {
                assert!(!codes.is_empty() && codes.len() % 4 == 0);
            }
            (Some(Unwind::Dwarf { rows, .. }), Os::Linux | Os::MacOs) => assert!(!rows.is_empty()),
            (unwind, os) => panic!("{os:?}: {unwind:?}"),
        }
    }
}
