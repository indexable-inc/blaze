use blaze_stencil::code::{CallSite, CodeRef, Handler, SlotsRef, UnwindRef};
use cranelift_codegen::ir;

use super::*;

fn stencil<'a>(
    unwind: UnwindRef<'a>,
    calls: &'a [CallSite],
    handlers: &'a [Handler],
    lines: &'a [(u32, ir::SourceLoc)],
) -> CodeRef<'a> {
    CodeRef {
        bytes: &[],
        align: 16,
        relocs: &[],
        unwind: Some(unwind),
        calls,
        handlers,
        lines,
        traps: &[],
        slots: SlotsRef { base: 6, offsets: &[] },
        frame: 0,
    }
}

/// As Cranelift's `systemv.rs` writes `Aarch64SetPointerAuth`: the value of the pseudo
/// register 34 (`RA_SIGN_STATE`).
#[test]
fn a_return_address_signing_rule_is_written_as_cranelifts() {
    for (signed, op) in [(false, gimli::DW_OP_lit0), (true, gimli::DW_OP_lit1)] {
        let mut expression = gimli::write::Expression::new();
        expression.op(op);
        assert_eq!(
            instruction(FrameRule::RaSignState(signed)),
            CallFrameInstruction::ValExpression(gimli::Register(34), expression)
        );
    }
}

#[test]
fn stencil_landing_pads_are_an_lsda_of_its_calls() {
    let rows = [(0, FrameRule::Cfa(7, 8)), (1, FrameRule::CfaOffset(16))];
    let calls = [CallSite { ret: 5, handlers: 0..0 }, CallSite { ret: 12, handlers: 0..2 }];
    let handlers =
        [Handler::Tag { tag: CLEANUP, landing: 20 }, Handler::Tag { tag: CATCH, landing: 30 }];
    let dwarf = UnwindRef::Dwarf { len: 40, rows: &rows };
    let made = FunctionUnwind::of_stencil(&stencil(dwarf, &calls, &handlers, &[])).unwrap();
    let expected = lsda::write(|| {
        [
            CallEntry { start: 4, landing_pad: 0, catch: false },
            CallEntry { start: 11, landing_pad: 20, catch: false },
            CallEntry { start: 11, landing_pad: 30, catch: true },
        ]
        .into_iter()
    });
    let frame = Frame::Rows(FrameRows { len: 40, rows: rows.to_vec() });
    assert_eq!(made, Some(FunctionUnwind { frame, landing_pads: Some(expected) }));

    let plain = FunctionUnwind::of_stencil(&stencil(dwarf, &calls[..1], &[], &[])).unwrap().unwrap();
    assert_eq!(plain.landing_pads, None);
    let every = [Handler::Default { landing: 20 }];
    assert!(FunctionUnwind::of_stencil(&stencil(dwarf, &calls, &every, &[])).is_err());
    let past = [CallSite { ret: 12, handlers: 0..3 }];
    assert!(FunctionUnwind::of_stencil(&stencil(dwarf, &past, &handlers, &[])).is_err());
    let none = CodeRef { unwind: None, ..stencil(dwarf, &[], &[], &[]) };
    assert_eq!(FunctionUnwind::of_stencil(&none).unwrap(), None);
}

/// x64's `UNWIND_INFO` is kept as given, and a guarded call returns to look at the exception.
#[test]
fn windows_stencil_code_has_pads_of_its_guarded_calls() {
    let loc = ir::SourceLoc::new(3);
    let lines = [(0, loc), (4, seh::guarded(loc)), (9, loc)];
    let calls = [CallSite { ret: 9, handlers: 0..0 }, CallSite { ret: 14, handlers: 0..0 }];
    let windows = UnwindRef::Windows { len: 20, info: &[1, 0, 0, 0] };
    let made = FunctionUnwind::of_stencil(&stencil(windows, &calls, &[], &lines)).unwrap();
    let mut pads = seh::Pads::default();
    pads.returning(9);
    let frame = Frame::Windows(seh::Frame { len: 20, info: vec![1, 0, 0, 0] });
    assert_eq!(made, Some(FunctionUnwind { frame, landing_pads: pads.finish() }));

    let handled = [CallSite { ret: 9, handlers: 0..1 }];
    let handlers = [Handler::Tag { tag: CATCH, landing: 16 }];
    assert!(FunctionUnwind::of_stencil(&stencil(windows, &handled, &handlers, &lines)).is_err());
}

fn header(info: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(info[at..at + 4].try_into().unwrap())
}

#[test]
fn an_arm64_xdata_record_is_its_header_and_codes() {
    // `save_fplr_x` and `end`, padded with `nop`.
    let codes = [0x81, 0xe4, 0xe3, 0xe3];
    let info = arm64_xdata(64, &codes).unwrap();
    assert_eq!(header(&info, 0), 16 | XDATA_E | 1 << 27);
    assert_eq!(&info[4..], &codes);

    let long = vec![0xe3; 4 * 40];
    let info = arm64_xdata(8, &long).unwrap();
    assert_eq!(header(&info, 0), 2 | XDATA_E);
    assert_eq!(header(&info, 4), 40 << 16);
    assert_eq!(&info[8..], &long[..]);

    assert!(arm64_xdata(6, &codes).is_err());
    assert!(arm64_xdata(4 * XDATA_WORDS, &codes).is_err());
    assert!(arm64_xdata(8, &[]).is_err());
    assert!(arm64_xdata(8, &codes[..3]).is_err());
    assert!(arm64_xdata(8, &vec![0xe3; 4 * 256]).is_err());

    let arm64 = UnwindRef::WindowsArm64 { len: 64, codes: &codes };
    let made = FunctionUnwind::of_stencil(&stencil(arm64, &[], &[], &[])).unwrap().unwrap();
    let frame = Frame::Windows(seh::Frame { len: 64, info: arm64_xdata(64, &codes).unwrap() });
    assert_eq!(made, FunctionUnwind { frame, landing_pads: None });
}

/// The instructions of the FDE gimli writes after `cie`, as v2 reads them back: past the CIE,
/// and past the FDE's length, CIE pointer, address and length.
fn fde_after(cie: &CommonInformationEntry, fde: FrameDescriptionEntry) -> Vec<u8> {
    let mut table = FrameTable::default();
    let id = table.add_cie(cie.clone());
    table.add_fde(id, fde);
    let mut eh_frame = EhFrame(EndianVec::new(RunTimeEndian::Little));
    table.write_eh_frame(&mut eh_frame).unwrap();
    let bytes = eh_frame.0.slice();
    let fde = 4 + header(bytes, 0) as usize;
    let end = fde + 4 + header(bytes, fde) as usize;
    assert_eq!(end, bytes.len());
    bytes[fde + 24..end].to_vec()
}

/// `fde_instructions` as it was before the cursor, indexing the bytes by hand.
fn fde_instructions_by_index(written: &[u8]) -> Option<std::ops::Range<usize>> {
    let length = |at: usize| -> Option<usize> {
        let bytes = written.get(at..at + 4)?;
        match u32::from_le_bytes(bytes.try_into().unwrap()) {
            u32::MAX => None,
            length => Some(at + 4 + length as usize),
        }
    };
    let fde = length(0)?;
    let end = length(fde)?;
    let start = fde + 4 + 4 + 16;
    (start <= end && end == written.len()).then_some(start..end)
}

/// The FDE's instructions are where indexing by hand found them, in what gimli writes, in
/// every prefix of it, with bytes after it, and in entries too short or 64-bit.
#[test]
fn fde_instructions_are_where_indexing_found_them() {
    let isa = crate::target().unwrap();
    let cie = isa.create_systemv_cie().expect("a DWARF ISA");
    let mut inputs = Vec::new();
    for len in [0, 16, 4096] {
        let mut fde = FrameDescriptionEntry::new(Address::Constant(0), len);
        // A multiple of every ISA's code alignment factor (AArch64's is 4): gimli refuses others.
        fde.add_instruction(4, CallFrameInstruction::CfaOffset(16));
        let mut table = FrameTable::default();
        let id = table.add_cie(cie.clone());
        table.add_fde(id, fde);
        let mut eh_frame = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh_frame).unwrap();
        let bytes = eh_frame.0.slice().to_vec();
        for cut in 0..bytes.len() {
            inputs.push(bytes[..cut].to_vec());
        }
        let mut longer = bytes.clone();
        longer.push(0);
        inputs.push(longer);
        inputs.push(bytes);
    }
    let short_fde = [&[4, 0, 0, 0, 0, 0, 0, 0][..], &[10, 0, 0, 0], &[0; 10]].concat();
    let wide = [&[4, 0, 0, 0, 0, 0, 0, 0][..], &[0xff; 4], &[0; 24]].concat();
    let exact = [&[0, 0, 0, 0][..], &[24, 0, 0, 0], &[0; 24]].concat();
    inputs.extend([short_fde, wide, exact]);
    for input in &inputs {
        assert_eq!(fde_instructions(input).ok(), fde_instructions_by_index(input), "{input:x?}");
    }
    assert!(inputs.iter().filter(|it| fde_instructions(it).is_ok()).count() >= 4);
}

/// What [`write_fde`] keeps of a function's entry is, byte for byte, what gimli writes of
/// it after the CIE the ISA makes, with that CIE's instructions and return register: the FDE
/// depends on the CIE's two factors alone. Rows of every rule, at every distance apart.
#[test]
fn an_fde_written_alone_is_the_one_written_after_the_isas_cie() {
    let isa = crate::target().unwrap();
    let cie = isa.create_systemv_cie().expect("a DWARF ISA");
    let reference = fde_after(&cie, FrameDescriptionEntry::new(Address::Constant(0), 0));
    assert!(reference.iter().all(|&it| it == gimli::DW_CFA_nop.0));
    // The factors, as the CIE has them: after its length, id, version and empty
    // augmentation, each one byte of LEB128.
    let mut written = EhFrame(EndianVec::new(RunTimeEndian::Little));
    let mut table = FrameTable::default();
    let id = table.add_cie(cie.clone());
    table.add_fde(id, FrameDescriptionEntry::new(Address::Constant(0), 0));
    table.write_eh_frame(&mut written).unwrap();
    let bytes = written.0.slice();
    let (code_align, data_align) = (u32::from(bytes[10]), i32::from((bytes[11] as i8) << 1 >> 1));
    assert!(bytes[10] < 0x80 && bytes[11] < 0x80);
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let (mut out, mut direct) = (Vec::new(), Vec::new());
    for _ in 0..500 {
        let mut fde = FrameDescriptionEntry::new(Address::Constant(0), 1 << 20);
        let mut rows = Vec::new();
        let mut offset = 0u32;
        for _ in 0..next() % 24 {
            let bits = next();
            offset += [0, 1, 63, 64, 255, 256, 65_535, 65_536][(bits % 8) as usize] * code_align;
            let register = ((bits >> 8) % 80) as u16;
            let scaled = data_align * ((bits >> 16) % 40) as i32;
            let rule = match (bits >> 24) % 12 {
                10 => FrameRule::RaSignState((bits >> 32) % 2 == 0),
                0 => FrameRule::Cfa(register, ((bits >> 32) % 300) as i32),
                1 => FrameRule::Cfa(register, -data_align.abs() * (1 + ((bits >> 32) % 40) as i32)),
                2 => FrameRule::CfaRegister(register),
                3 => FrameRule::CfaOffset(((bits >> 32) % 300) as i32),
                4 => FrameRule::Offset(register, scaled),
                5 => FrameRule::Offset(register, -scaled),
                6 => FrameRule::Restore(register),
                7 => FrameRule::SameValue(register),
                8 => FrameRule::Undefined(register),
                9 => FrameRule::ArgsSize((bits >> 32) as u32 % 1000),
                _ => [FrameRule::RememberState, FrameRule::RestoreState][(bits >> 32) as usize % 2],
            };
            fde.add_instruction(offset, instruction(rule));
            rows.push((offset, rule));
        }
        let reference = fde_after(&cie, fde.clone());
        let before = out.len();
        write_fde(fde, code_align, data_align, &mut out).unwrap();
        assert_eq!(out[before..], reference[..]);
        let before = direct.len();
        let frame = Frame::Rows(FrameRows { len: 1 << 20, rows });
        Fde(&frame).write(code_align, data_align, &mut direct).unwrap();
        assert_eq!(direct[before..], reference[..], "{frame:?}");
    }
}

/// Rows gimli refuses are refused, and leave nothing behind.
#[test]
fn rows_gimli_refuses_are_refused() {
    let refused = [
        vec![(4, FrameRule::CfaOffset(16)), (0, FrameRule::CfaOffset(8))],
        vec![(0, FrameRule::Offset(6, -12))],
        vec![(0, FrameRule::Cfa(7, -4))],
    ];
    for rows in refused {
        let mut out = vec![0xaa];
        assert!(write_rows(&rows, 1, -8, &mut out).is_err(), "{rows:?}");
        assert_eq!(out, [0xaa]);
    }
    let mut out = Vec::new();
    assert!(write_rows(&[(3, FrameRule::RememberState)], 4, -8, &mut out).is_err());
}

/// An ISA of `triple` as the process makes one, with frame pointers kept as v2's code has
/// them.
fn isa_of(triple: &str) -> cranelift_codegen::isa::OwnedTargetIsa {
    use cranelift_codegen::settings::{self, Configurable};
    let mut flags = settings::builder();
    flags.set("is_pic", "true").unwrap();
    flags.set("preserve_frame_pointers", "true").unwrap();
    cranelift_codegen::isa::lookup(triple.parse().unwrap())
        .unwrap()
        .finish(settings::Flags::new(flags))
        .unwrap()
}

/// A function that keeps `ints` integers and `floats` floats live across a call, so that
/// the register allocator saves callee-saved registers of both kinds, and a frame of
/// `stack` bytes.
fn saving(isa: &dyn TargetIsa, ints: usize, floats: usize, stack: u32) -> ir::Function {
    use cranelift_codegen::ir::{InstBuilder, StackSlotData, StackSlotKind};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    let mut sig = ir::Signature::new(isa.default_call_conv());
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::F64));
    sig.returns.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::F64));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig.clone());
    let mut context = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut func, &mut context);
    let entry = builder.create_block();
    builder.append_block_params_for_function_params(entry);
    builder.switch_to_block(entry);
    let (int, float) = (builder.block_params(entry)[0], builder.block_params(entry)[1]);
    if stack > 0 {
        let slot = builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, stack, 4));
        builder.ins().stack_store(types::I64, int, slot, 0);
    }
    let ints: Vec<_> = (0..ints as i64).map(|at| builder.ins().imul_imm(int, at + 3)).collect();
    let floats: Vec<_> = (0..floats).map(|_| builder.ins().fadd(float, float)).collect();
    let signature = builder.import_signature(sig);
    let name = builder.func.declare_imported_user_function(ir::UserExternalName::new(0, 0));
    let callee = builder.import_function(ir::ExtFuncData {
        name: ir::ExternalName::user(name),
        signature,
        colocated: false,
        patchable: false,
    });
    let call = builder.ins().call(callee, &[int, float]);
    let (mut int, mut float) = (builder.inst_results(call)[0], builder.inst_results(call)[1]);
    for value in ints {
        int = builder.ins().iadd(int, value);
    }
    for value in floats {
        float = builder.ins().fadd(float, value);
    }
    builder.ins().return_(&[int, float]);
    builder.seal_all_blocks();
    builder.finalize(isa.frontend_config());
    func
}

/// The rows [`SystemV::rows`] makes of a function's unwind instructions are, as an FDE,
/// Cranelift's own: on x86-64 and AArch64, of functions that save no, some and every
/// callee-saved register of either kind, with frames of no to a megabyte of slots, and with
/// AArch64's return addresses signed.
#[test]
fn system_v_rows_are_cranelifts() {
    let triples = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin"];
    let mut isas: Vec<_> = triples.iter().map(|it| isa_of(it)).collect();
    isas.push({
        use cranelift_codegen::settings::{self, Configurable};
        let mut flags = settings::builder();
        flags.set("preserve_frame_pointers", "true").unwrap();
        let mut isa = cranelift_codegen::isa::lookup("aarch64-unknown-linux-gnu".parse().unwrap()).unwrap();
        isa.enable("sign_return_address").unwrap();
        isa.finish(settings::Flags::new(flags)).unwrap()
    });
    for isa in &isas {
        let isa = &**isa;
        for (ints, floats, stack) in [(0, 0, 0), (2, 0, 0), (12, 0, 64), (0, 12, 0), (24, 24, 4096), (40, 40, 1 << 20)] {
            let mut context = Context::for_function(saving(isa, ints, floats, stack));
            context.compile(isa, &mut Default::default()).unwrap();
            let Some(UnwindInfo::SystemV(info)) = context.create_unwind_info(isa).unwrap() else {
                panic!("{}: no System V unwind information", isa.triple());
            };
            let made = FunctionUnwind::of(isa, &context).unwrap().unwrap();
            assert!(matches!(made.frame, Frame::Rows(_)), "{}", isa.triple());
            let address = Address::Constant(0);
            assert_eq!(made.fde(address), Some(info.to_fde(address)), "{} {ints} {floats} {stack}", isa.triple());
        }
    }
}

/// Factors no CIE byte holds are refused, never wrapped.
#[test]
fn an_fde_of_factors_out_of_range_is_refused() {
    let fde = || FrameDescriptionEntry::new(Address::Constant(0), 0);
    let mut out = Vec::new();
    assert!(write_fde(fde(), 256, -8, &mut out).is_err());
    assert!(write_fde(fde(), 1, -129, &mut out).is_err());
    assert!(out.is_empty());
}
