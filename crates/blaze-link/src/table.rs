//! The jump table: the patch table of #275 (docs/v2/plan/lanes/link/notes.md, "The jump table").
//!
//! A call to a function through its slot goes to the slot's stub, which loads the table
//! cell (`Class::Table`'s first word: the address of the current table), then the slot's
//! entry of that table, and jumps there. A link that moves a function changes one entry; a
//! running program switches every entry at once by storing another table's address in the
//! table cell (blaze-live), and rolls a patch back by storing the old one.
//!
//! The stubs use only registers a call may clobber and no argument is passed in: `r11` on
//! x86-64 (neither SysV nor Win64 nor Cranelift passes arguments in it; `r10` is SysV's
//! static chain), `x16` and `x17` on AArch64 (the intra-procedure-call registers AAPCS64
//! sets aside for veneers).
//!
//! An image linked with the execution log (`Output::exec_log`, fastcompile-ident-select 9) also
//! has a log stub per slot after the slot stubs, and the cell's piece is the header
//! [`blaze_live::patch::exec`] lays out, with a log table whose word `s` starts at slot `s`'s
//! log stub. The log stub puts its slot in the same scratch register and branches to one log
//! routine, which marks the slot in the map, copies the current generation's word into the
//! log table and jumps there. The routine needs one register more on x86-64 than `r11` and
//! one on AArch64 than `x16` and `x17`: it saves and restores them on the stack, so every
//! argument register, the stack's arguments and the flags reach the function as they were.

use blaze_codegen::Arch;
use blaze_graph::par;
use blaze_live::patch::exec;

use crate::model::Apply;

/// A fixup of a generated stub: at `at`, to the table cell or a GOT cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StubFixup {
    pub at: u64,
    pub apply: Apply,
    pub addend: i64,
}

/// The size of one slot's stub.
pub(crate) fn stub_size(arch: Arch) -> u64 {
    match arch {
        Arch::X86_64 => 16,
        Arch::Aarch64 => 24,
        Arch::Wasm32 => 0,
    }
}

/// `mov r11, [rip + cell]; jmp [r11 + disp32]; int3; int3`, the displacement at 10.
const X86_STUB: [u8; 16] = [0x4c, 0x8b, 0x1d, 0, 0, 0, 0, 0x41, 0xff, 0xa3, 0, 0, 0, 0, 0xcc, 0xcc];
const X86_STUB_FIXUPS: [StubFixup; 1] = [StubFixup { at: 3, apply: Apply::Rel32, addend: -4 }];

/// adrp x16, cell; ldr x16, [x16, :lo12:cell]; movz x17, #low; movk x17, #high, lsl #16;
/// ldr x16, [x16, x17]; br x16: a slot from 2^21.
const A64_STUB: [u32; 6] = [0x9000_0010, 0xf940_0210, 0xd280_0011, 0xf2a0_0011, 0xf871_6a10, 0xd61f_0200];
/// `ldr x16, [x16, #imm12 * 8]`, the slot's entry loaded at an unsigned scaled offset.
const A64_LDR_X16: u32 = 0xf940_0210;
/// `add x16, x16, #imm12, lsl #12`.
const A64_ADD_X16_LSL12: u32 = 0x9140_0210;
const A64_BR_X16: u32 = 0xd61f_0200;
/// `brk #0`: the stub's words past its `br`, never run.
const A64_BRK: u32 = 0xd420_0000;
const A64_PAGE_FIXUPS: [StubFixup; 2] = [
    StubFixup { at: 0, apply: Apply::A64Page21, addend: 0 },
    StubFixup { at: 4, apply: Apply::A64Lo12 { shift: 3 }, addend: 0 },
];

/// The fixups of every slot's stub, from the stub's start: each to the table cell.
pub(crate) fn stub_fixups(arch: Arch) -> &'static [StubFixup] {
    match arch {
        Arch::X86_64 => &X86_STUB_FIXUPS,
        Arch::Aarch64 => &A64_PAGE_FIXUPS,
        Arch::Wasm32 => &[],
    }
}

/// Writes the stub of `slot` into `out`, [`stub_size`] bytes; its fixups are [`stub_fixups`].
pub(crate) fn write_stub(arch: Arch, slot: u32, out: &mut [u8]) {
    let offset = u64::from(slot) * 8;
    match arch {
        Arch::X86_64 => {
            let disp = u32::try_from(offset).expect("a slot below 2^29");
            out[..16].copy_from_slice(&X86_STUB);
            out[10..14].copy_from_slice(&disp.to_le_bytes());
        }
        Arch::Aarch64 => {
            // The cell, then the slot's entry: two dependent loads in every form. Below 4096
            // slots the entry's offset fits the load (4 instructions); below 2^21 one `add`
            // takes its high 12 bits (5); past that `movz`/`movk` build it (6).
            let [adrp, cell] = [A64_STUB[0], A64_STUB[1]];
            let words = if offset < 1 << 15 {
                let entry = A64_LDR_X16 | ((offset >> 3) as u32) << 10;
                [adrp, cell, entry, A64_BR_X16, A64_BRK, A64_BRK]
            } else if offset < 1 << 24 {
                let page = A64_ADD_X16_LSL12 | ((offset >> 12) as u32) << 10;
                let entry = A64_LDR_X16 | (((offset & 0xfff) >> 3) as u32) << 10;
                [adrp, cell, page, entry, A64_BR_X16, A64_BRK]
            } else {
                let mut words = A64_STUB;
                words[2] |= ((offset & 0xffff) as u32) << 5;
                words[3] |= (((offset >> 16) & 0xffff) as u32) << 5;
                words
            };
            for (to, word) in out[..24].chunks_exact_mut(4).zip(words) {
                to.copy_from_slice(&word.to_le_bytes());
            }
        }
        Arch::Wasm32 => {}
    }
}

/// The stubs of slots from 0, one after another, written in parallel over the whole of `out`
/// (a multiple of [`stub_size`] long).
pub(crate) fn stubs(arch: Arch, out: &mut [u8]) {
    let size = stub_size(arch) as usize;
    if size == 0 {
        return;
    }
    par::for_each(par::enumerate(par::chunks_mut(out, size)), |(slot, out)| {
        write_stub(arch, slot as u32, out);
    });
}

/// The stub of `slot`: its bytes and fixups.
#[cfg(test)]
pub(crate) fn stub(arch: Arch, slot: u32) -> (Vec<u8>, &'static [StubFixup]) {
    let mut code = vec![0; stub_size(arch) as usize];
    write_stub(arch, slot, &mut code);
    (code, stub_fixups(arch))
}

/// The size of one slot's log stub: it puts the slot in a scratch register and branches to the
/// log routine ([`log_stubs`]).
pub(crate) fn log_stub_size(arch: Arch) -> u64 {
    match arch {
        Arch::X86_64 | Arch::Aarch64 => 16,
        Arch::Wasm32 => 0,
    }
}

/// The bytes of the log stubs of a table of `slots` slots and the routine after them.
pub(crate) fn log_size(arch: Arch, slots: u64) -> u64 {
    match arch {
        Arch::X86_64 => slots * 16 + X86_LOG.len() as u64,
        Arch::Aarch64 => slots * 16 + A64_LOG.len() as u64 * 4,
        Arch::Wasm32 => 0,
    }
}

/// The most slots AArch64's log stubs serve: each branches to the routine after them with a
/// `B`, which reaches 128 MiB.
pub(crate) const A64_LOG_SLOTS: u64 = ((1 << 27) - 64) / 16;

/// The log routine, entered with the slot in `r11`; every other register is as the caller
/// left it, and is again at the jump: `rax` and `rcx` are saved on the stack below the
/// return address and taken back, and no instruction changes the flags.
///
/// ```text
///  0  push rax
///  1  push rcx
///  2  mov rax, [rip + map]
///  9  mov byte [rax + r11 + 16], 1
/// 15  mov rax, [rip + current]
/// 22  mov rax, [rax + r11*8]
/// 26  lea rcx, [rip + log table]
/// 33  mov [rcx + r11*8], rax
/// 37  mov r11, rax
/// 40  pop rcx
/// 41  pop rax
/// 42  jmp r11
/// ```
const X86_LOG: [u8; 48] = [
    0x50, 0x51, 0x48, 0x8b, 0x05, 0, 0, 0, 0, 0x42, 0xc6, 0x44, 0x18, 0x10, 0x01, 0x48, 0x8b, 0x05, 0, 0, 0, 0, 0x4a, 0x8b,
    0x04, 0xd8, 0x48, 0x8d, 0x0d, 0, 0, 0, 0, 0x4a, 0x89, 0x04, 0xd9, 0x49, 0x89, 0xc3, 0x59, 0x58, 0x41, 0xff, 0xe3, 0xcc,
    0xcc, 0xcc,
];
const X86_LOG_FIXUPS: [StubFixup; 3] = [
    StubFixup { at: 5, apply: Apply::Rel32, addend: exec::MAP as i64 - 4 },
    StubFixup { at: 18, apply: Apply::Rel32, addend: exec::CURRENT as i64 - 4 },
    StubFixup { at: 29, apply: Apply::Rel32, addend: exec::HEADER as i64 - 4 },
];

/// The log routine, entered with the slot in `x16`; `x0` is saved on the stack (16 bytes,
/// keeping `sp` aligned) and taken back, and only `x16` and `x17` are left changed, as the slot
/// stub leaves them.
///
/// ```text
///  0  str  x0, [sp, #-16]!
///  4  adrp x17, map
///  8  ldr  x17, [x17, :lo12:map]
/// 12  add  x17, x17, x16
/// 16  movz w0, #1
/// 20  strb w0, [x17, #16]
/// 24  adrp x17, current
/// 28  ldr  x17, [x17, :lo12:current]
/// 32  ldr  x17, [x17, x16, lsl #3]
/// 36  adrp x0, log table
/// 40  add  x0, x0, :lo12:log table
/// 44  str  x17, [x0, x16, lsl #3]
/// 48  ldr  x0, [sp], #16
/// 52  br   x17
/// 56  brk  #0; brk #0
/// ```
const A64_LOG: [u32; 16] = [
    0xf81f_0fe0,
    0x9000_0011,
    0xf940_0231,
    0x8b10_0231,
    0x5280_0020,
    0x3900_4220,
    0x9000_0011,
    0xf940_0231,
    0xf870_7a31,
    0x9000_0000,
    0x9100_0000,
    0xf830_7811,
    0xf841_07e0,
    0xd61f_0220,
    0xd420_0000,
    0xd420_0000,
];
const A64_LOG_FIXUPS: [StubFixup; 6] = [
    StubFixup { at: 4, apply: Apply::A64Page21, addend: exec::MAP as i64 },
    StubFixup { at: 8, apply: Apply::A64Lo12 { shift: 3 }, addend: exec::MAP as i64 },
    StubFixup { at: 24, apply: Apply::A64Page21, addend: exec::CURRENT as i64 },
    StubFixup { at: 28, apply: Apply::A64Lo12 { shift: 3 }, addend: exec::CURRENT as i64 },
    StubFixup { at: 36, apply: Apply::A64Page21, addend: exec::HEADER as i64 },
    StubFixup { at: 40, apply: Apply::A64Lo12 { shift: 0 }, addend: exec::HEADER as i64 },
];

/// The fixups of the log routine, from its start ([`log_routine`]): each to the table's
/// header, plus the addend.
pub(crate) fn log_fixups(arch: Arch) -> &'static [StubFixup] {
    match arch {
        Arch::X86_64 => &X86_LOG_FIXUPS,
        Arch::Aarch64 => &A64_LOG_FIXUPS,
        Arch::Wasm32 => &[],
    }
}

/// Where the log routine starts in the log stubs of `slots` slots.
pub(crate) fn log_routine(arch: Arch, slots: u64) -> u64 {
    slots * log_stub_size(arch)
}

/// Writes the log stub of `slot` into `out`, [`log_stub_size`] bytes, for a routine `to`
/// bytes after the stub's start. It has no fixups: the branch is within the piece.
///
/// x86-64: `mov r11d, slot; jmp routine`. AArch64: `movz x16, #low; movk x16, #high, lsl #16;
/// b routine`. Both pad with traps.
pub(crate) fn write_log_stub(arch: Arch, slot: u32, to: u64, out: &mut [u8]) {
    match arch {
        Arch::X86_64 => {
            let rel = i32::try_from(to as i64 - 11).expect("a log routine within 2 GiB");
            out[..16].copy_from_slice(&[0x41, 0xbb, 0, 0, 0, 0, 0xe9, 0, 0, 0, 0, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc]);
            out[2..6].copy_from_slice(&slot.to_le_bytes());
            out[7..11].copy_from_slice(&rel.to_le_bytes());
        }
        Arch::Aarch64 => {
            let words = [
                0xd280_0010 | (slot & 0xffff) << 5,
                0xf2a0_0010 | (slot >> 16) << 5,
                0x1400_0000 | (((to - 8) / 4) as u32 & 0x03ff_ffff),
                0xd420_0000,
            ];
            for (into, word) in out[..16].chunks_exact_mut(4).zip(words) {
                into.copy_from_slice(&word.to_le_bytes());
            }
        }
        Arch::Wasm32 => {}
    }
}

/// The log stubs of slots from 0 and the log routine after them, over the whole of `out`
/// ([`log_size`] long), the stubs written in parallel. Its fixups are [`log_fixups`], at
/// [`log_routine`].
pub(crate) fn log_stubs(arch: Arch, slots: u64, out: &mut [u8]) {
    let size = log_stub_size(arch) as usize;
    if size == 0 {
        return;
    }
    let (stubs, routine) = out.split_at_mut(slots as usize * size);
    match arch {
        Arch::X86_64 => routine.copy_from_slice(&X86_LOG),
        Arch::Aarch64 => {
            for (into, word) in routine.chunks_exact_mut(4).zip(A64_LOG) {
                into.copy_from_slice(&word.to_le_bytes());
            }
        }
        Arch::Wasm32 => {}
    }
    let end = slots * size as u64;
    par::for_each(par::enumerate(par::chunks_mut(stubs, size)), |(slot, out)| {
        write_log_stub(arch, slot as u32, end - slot as u64 * size as u64, out);
    });
}

/// The size of one PLT stub: a jump through an import's GOT cell.
pub(crate) fn plt_size(arch: Arch) -> u64 {
    match arch {
        Arch::X86_64 | Arch::Aarch64 => 16,
        Arch::Wasm32 => 0,
    }
}

/// `jmp [rip + cell]`, padded with `int3`.
const X86_PLT: [u8; 16] = [0xff, 0x25, 0, 0, 0, 0, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc];
const X86_PLT_FIXUPS: [StubFixup; 1] = [StubFixup { at: 2, apply: Apply::Rel32, addend: -4 }];
/// adrp x16, cell; ldr x17, [x16, :lo12:cell]; br x17; brk #0.
const A64_PLT: [u8; 16] = words_le([0x9000_0010, 0xf940_0211, 0xd61f_0220, 0xd420_0000]);

const fn words_le(words: [u32; 4]) -> [u8; 16] {
    let mut out = [0; 16];
    let mut at = 0;
    while at < 4 {
        let bytes = words[at].to_le_bytes();
        out[at * 4] = bytes[0];
        out[at * 4 + 1] = bytes[1];
        out[at * 4 + 2] = bytes[2];
        out[at * 4 + 3] = bytes[3];
        at += 1;
    }
    out
}

/// A PLT stub: its bytes, and the fixups that point it at the GOT cell. Both static: making a
/// stub allocates nothing.
pub(crate) fn plt(arch: Arch) -> (&'static [u8], &'static [StubFixup]) {
    match arch {
        Arch::X86_64 => (&X86_PLT, &X86_PLT_FIXUPS),
        Arch::Aarch64 => (&A64_PLT, &A64_PAGE_FIXUPS),
        Arch::Wasm32 => (&[], &[]),
    }
}

/// How many slots a table made for `functions` has room for: with room to grow for an
/// incremental image, as many as there are otherwise.
pub(crate) fn capacity(functions: u64, incremental: bool) -> u64 {
    if incremental { (functions * 2).max(functions + (1 << 16)) } else { functions }
}

/// A program staged with the execution log, and its table's header and stubs read back from
/// the staged state, as relocated: what each format's tests check for each arch.
#[cfg(test)]
pub(crate) mod linked {
    use blaze_codegen::{Arch, CompiledFunction, InstanceKey, RelocKind, Relocation, Target};
    use blaze_core::FileName;
    use blaze_ids::ContentId;
    use blaze_live::patch::exec;

    use super::{log_routine, log_stub_size, stub_size};
    use crate::state::State;
    use crate::symbols::SymKey;
    use crate::{Change, Image, Live, Output, OutputKind};

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    /// Function 1 calls function 2 through its slot; function 2 returns.
    fn function(arch: Arch, n: u8) -> CompiledFunction {
        let words = |words: [u32; 2]| words.iter().flat_map(|it| it.to_le_bytes()).collect::<Vec<u8>>();
        let call = |kind, at, addend| vec![Relocation { at, kind, target: Target::Function(key(2)), addend }];
        let (code, relocs) = match (arch, n) {
            (Arch::X86_64, 1) => (vec![0xe8, 0, 0, 0, 0, 0xc3], call(RelocKind::PcRel32, 1, -4)),
            (Arch::X86_64, _) => (vec![0xb8, 1, 0, 0, 0, 0xc3], Vec::new()),
            // bl g; ret.
            (_, 1) => (words([0x9400_0000, 0xd65f_03c0]), call(RelocKind::Call26, 0, 0)),
            // mov w0, #1; ret.
            _ => (words([0x5280_0020, 0xd65f_03c0]), Vec::new()),
        };
        CompiledFunction {
            key: key(n),
            arch,
            align: 16,
            code,
            relocs: relocs.into(),
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
            coverage: None,
            profile: None,
        }
    }

    /// `len` bytes at `addr` of the staged state.
    fn read(state: &State, addr: u64, len: usize) -> Vec<u8> {
        let space = state
            .spaces
            .iter()
            .find(|it| it.region.addr <= addr && addr + len as u64 <= it.region.addr + it.region.size)
            .unwrap_or_else(|| panic!("no region has {addr:#x}"));
        let mut out = vec![0; len];
        space.bytes.copy_to((addr - space.region.addr) as usize, &mut out);
        out
    }

    fn word(state: &State, addr: u64) -> u64 {
        u64::from_le_bytes(read(state, addr, 8).try_into().unwrap())
    }

    /// The header's address, slot count, the stubs' address and the staged state of a
    /// program linked as `output` with the log.
    pub(crate) struct Logged {
        pub header: u64,
        pub slots: u64,
        pub stubs: u64,
        pub state: State,
    }

    impl Logged {
        pub(crate) fn word(&self, addr: u64) -> u64 {
            word(&self.state, addr)
        }

        pub(crate) fn bytes(&self, addr: u64, len: usize) -> Vec<u8> {
            read(&self.state, addr, len)
        }
    }

    /// Stages the program into an image linked as `output` with the execution log and a live
    /// descriptor, and checks the header's words: the cell on the log table, the current
    /// generation and the loader's table at the cells, the map at the private map, the log
    /// stubs after the slot stubs, the slot count and stride, and each log table word at its
    /// slot's log stub.
    pub(crate) fn logged(output: Output) -> Logged {
        let arch = output.arch;
        let output = Output { incremental: true, live: Some(Live), exec_log: true, ..output };
        let OutputKind::Executable { entry } = &output.kind else { panic!("an executable") };
        let entry = entry.clone();
        let (start, g) = (function(arch, 1), function(arch, 2));
        let mut image = Image::new(FileName::new("out/exec").unwrap(), output).unwrap();
        let change = Change {
            added: vec![&start, &g],
            names: vec![(entry, Target::Direct(key(1)))],
            ..Change::default()
        };
        let staged = image.stage(change).unwrap();
        let find = |key: SymKey| {
            let sym = image.symbols.find(key).expect("the table's symbol");
            staged.state.addr_of(sym).expect("placed")
        };
        let (header, cells, stubs) = (find(SymKey::Generation), find(SymKey::Cells), find(SymKey::Stubs));
        let g_slot = image.symbols.find_slot(&key(2)).expect("g's slot");
        let g_slot = (staged.state.addr_of(g_slot).expect("placed") - stubs) / stub_size(arch);
        let state = staged.state;
        let at = |field: u64| word(&state, header + field);
        let fields = read(&state, header + exec::SLOTS, 8);
        let slots = u64::from(u32::from_le_bytes(fields[..4].try_into().unwrap()));
        let stride = u64::from(u32::from_le_bytes(fields[4..].try_into().unwrap()));
        assert!(slots >= 2 && stride == log_stub_size(arch), "{slots} slots, stride {stride}");
        let log_stubs = stubs + slots * stub_size(arch);
        assert_eq!(at(exec::CELL), header + exec::log_word(0), "a process starts on the log table");
        assert_eq!((at(exec::CURRENT), at(exec::LOADER)), (cells, cells));
        assert_eq!(at(exec::MAP), header + exec::private_map(slots));
        assert_eq!(at(exec::STUBS), log_stubs);
        for slot in [0, 1, slots - 1] {
            assert_eq!(at(exec::log_word(slot)), log_stubs + slot * stride, "slot {slot}");
        }
        let map = read(&state, header + exec::private_map(slots), (exec::REGION_HEADER + slots) as usize);
        assert!(map.iter().all(|it| *it == 0), "nothing is marked before the program runs");
        assert!(g_slot < slots);
        assert_eq!(log_routine(arch, slots) % 16, 0);
        let logged = Logged { header, slots, stubs, state };
        log_code(&logged, arch);
        logged
    }

    /// The target of the AArch64 `ADRP` at `p`.
    pub(crate) fn adrp(insn: u32, p: u64) -> u64 {
        assert_eq!(insn & 0x9f00_0000, 0x9000_0000, "{insn:#x} is an ADRP");
        let imm = (((insn >> 5) & 0x7_ffff) << 2 | (insn >> 29) & 3) as u64;
        let imm = ((imm << 43) as i64 >> 43) as u64;
        (p & !0xfff).wrapping_add(imm << 12)
    }

    /// Checks the log stubs as relocated: each puts its slot in the scratch register and
    /// reaches the routine, whose loads and stores reach the header's map, current table and
    /// log table, every other byte as [`super::log_stubs`] writes it.
    fn log_code(logged: &Logged, arch: Arch) {
        let (header, slots) = (logged.header, logged.slots);
        let log_stubs = logged.stubs + slots * stub_size(arch);
        let routine = log_stubs + log_routine(arch, slots);
        let le32 = |bytes: &[u8], at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        for slot in [0, 1, slots - 1] {
            let at = log_stubs + slot * log_stub_size(arch);
            let stub = logged.bytes(at, 16);
            match arch {
                Arch::X86_64 => {
                    assert_eq!((&stub[..2], stub[6]), (&[0x41, 0xbb][..], 0xe9), "mov r11d, slot; jmp");
                    assert_eq!(u64::from(le32(&stub, 2)), slot);
                    assert_eq!((at + 11).wrapping_add(le32(&stub, 7) as i32 as i64 as u64), routine, "slot {slot}");
                }
                _ => {
                    let (low, high) = (u64::from((le32(&stub, 0) >> 5) & 0xffff), u64::from((le32(&stub, 4) >> 5) & 0xffff));
                    assert_eq!(low | high << 16, slot);
                    assert_eq!(at + 8 + u64::from(le32(&stub, 8) & 0x03ff_ffff) * 4, routine, "slot {slot}");
                }
            }
        }
        match arch {
            Arch::X86_64 => {
                let code = logged.bytes(routine, super::X86_LOG.len());
                let rip = |disp: usize| (routine + disp as u64 + 4).wrapping_add(le32(&code, disp) as i32 as i64 as u64);
                assert_eq!(rip(5), header + exec::MAP);
                assert_eq!(rip(18), header + exec::CURRENT);
                assert_eq!(rip(29), header + exec::log_word(0));
                for (at, (got, want)) in code.iter().zip(super::X86_LOG).enumerate() {
                    if !matches!(at, 5..9 | 18..22 | 29..33) {
                        assert_eq!(*got, want, "byte {at} of the routine");
                    }
                }
            }
            _ => {
                let code = logged.bytes(routine, super::A64_LOG.len() * 4);
                let word = |n: usize| le32(&code, n * 4);
                let lo12 = |n: usize| u64::from((word(n) >> 10) & 0xfff);
                assert_eq!(adrp(word(1), routine + 4) + lo12(2) * 8, header + exec::MAP);
                assert_eq!(adrp(word(6), routine + 24) + lo12(7) * 8, header + exec::CURRENT);
                assert_eq!(adrp(word(9), routine + 36) + lo12(10), header + exec::log_word(0));
                for (n, want) in super::A64_LOG.iter().enumerate() {
                    let got = match n {
                        1 | 6 | 9 => word(n) & !(0x7_ffff << 5 | 3 << 29),
                        2 | 7 | 10 => word(n) & !(0xfff << 10),
                        _ => word(n),
                    };
                    assert_eq!(got, *want, "word {n} of the routine");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::linked::adrp;
    use super::*;
    use crate::reloc::{Ctx, apply};

    fn relocate(arch: Arch, code: &mut [u8], fixups: &[StubFixup], p: u64, s: u64) {
        for fixup in fixups {
            let ctx = Ctx {
                arch,
                format: crate::Format::Elf,
                s,
                a: fixup.addend,
                p: p + fixup.at,
                g: 0,
                m: 0,
                base: 0,
                tls: None,
                tp: None,
            };
            apply(fixup.apply, &ctx, &mut code[fixup.at as usize..]).unwrap();
        }
    }

    #[test]
    fn an_x86_stub_loads_the_cell_then_jumps_through_the_slot() {
        let (mut code, fixups) = stub(Arch::X86_64, 5);
        assert_eq!(code.len() as u64, stub_size(Arch::X86_64));
        relocate(Arch::X86_64, &mut code, &fixups, 0x1000, 0x5000);
        // mov r11, [rip + disp]: rip is the end of the instruction, 0x1007.
        assert_eq!(&code[..3], [0x4c, 0x8b, 0x1d]);
        assert_eq!(i32::from_le_bytes(code[3..7].try_into().unwrap()), 0x5000 - 0x1007);
        // jmp [r11 + 5 * 8].
        assert_eq!(&code[7..10], [0x41, 0xff, 0xa3]);
        assert_eq!(u32::from_le_bytes(code[10..14].try_into().unwrap()), 40);
    }

    /// Each form of the AArch64 stub, run by a small interpreter of the instructions it uses:
    /// it loads the cell, then the slot's entry of the table the cell holds, then branches
    /// there, and nothing else. 4 instructions below 4096 slots, 5 below 2^21, 6 to 2^29.
    #[test]
    fn an_aarch64_stub_reaches_any_of_2_pow_29_slots() {
        let forms = [(0, 4), (1, 4), (4095, 4), (4096, 5), (0x1_2345, 5), ((1 << 21) - 1, 5)];
        for (slot, run) in forms.into_iter().chain([(1 << 21, 6), ((1 << 29) - 1, 6)]) {
            let (mut code, fixups) = stub(Arch::Aarch64, slot);
            assert_eq!(code.len() as u64, stub_size(Arch::Aarch64));
            relocate(Arch::Aarch64, &mut code, &fixups, 0x10_0000, 0x40_0008);
            let word = |at: usize| u32::from_le_bytes(code[at * 4..at * 4 + 4].try_into().unwrap());
            assert_eq!(word(0) & 0x9f00_001f, 0x9000_0010, "adrp x16");
            // The cell's low 12 bits, scaled for the 8-byte load.
            assert_eq!(word(1), A64_LDR_X16 | (8 >> 3) << 10, "ldr x16, [x16, :lo12:cell]");
            let (imm12, imm16) = (|it: u32| (it >> 10) & 0xfff, |it: u32| (it >> 5) & 0xffff);
            let table = 0x7000_0000u64;
            let (mut x16, mut x17, mut loaded) = (table, 0u64, None);
            let mut at = 2;
            loop {
                let it = word(at);
                at += 1;
                match it {
                    A64_BR_X16 => break,
                    _ if it & 0xffc0_03ff == A64_ADD_X16_LSL12 => x16 += u64::from(imm12(it)) << 12,
                    _ if it & 0xffe0_001f == 0xd280_0011 => x17 = u64::from(imm16(it)),
                    _ if it & 0xffe0_001f == 0xf2a0_0011 => x17 |= u64::from(imm16(it)) << 16,
                    _ if it & 0xffc0_03ff == A64_LDR_X16 && loaded.is_none() => {
                        loaded = Some(x16 + u64::from(imm12(it)) * 8);
                    }
                    0xf871_6a10 if loaded.is_none() => loaded = Some(x16 + x17),
                    other => panic!("slot {slot}: {other:#010x} at word {}", at - 1),
                }
            }
            assert_eq!(loaded, Some(table + u64::from(slot) * 8), "slot {slot} loads its entry");
            assert_eq!(at, run, "slot {slot} runs {run} instructions");
            assert!((at..6).all(|it| word(it) == A64_BRK), "slot {slot}: the words past `br` trap");
        }
    }

    #[test]
    fn the_bulk_stubs_are_each_slots_stub() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let size = stub_size(arch) as usize;
            let mut all = vec![0; size * 9000];
            stubs(arch, &mut all);
            assert_eq!(all.len(), size * 9000);
            for slot in [0u32, 1, 4095, 4096, 8191, 8999] {
                let (one, _) = stub(arch, slot);
                assert_eq!(&all[slot as usize * size..][..size], &one[..], "{arch:?} slot {slot}");
            }
        }
        let (x86, fixups) = plt(Arch::X86_64);
        assert_eq!((x86.len() as u64, &x86[..2], fixups[0].at), (plt_size(Arch::X86_64), &[0xff, 0x25][..], 2));
        let (a64, _) = plt(Arch::Aarch64);
        assert_eq!(u32::from_le_bytes(a64[8..12].try_into().unwrap()), 0xd61f_0220);
    }

    fn le32(code: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(code[at..at + 4].try_into().unwrap())
    }

    /// The log stubs and routine of `slots` slots, relocated as linked at `p` with the header
    /// at `header`.
    fn log_relocated(arch: Arch, slots: u64, p: u64, header: u64) -> Vec<u8> {
        let mut code = vec![0; log_size(arch, slots) as usize];
        log_stubs(arch, slots, &mut code);
        let at = log_routine(arch, slots);
        relocate(arch, &mut code[at as usize..], log_fixups(arch), p + at, header);
        code
    }

    #[test]
    fn an_x86_log_stub_puts_its_slot_in_r11_and_jumps_to_the_routine() {
        let slots = 300;
        let code = log_relocated(Arch::X86_64, slots, 0x1000, 0x9_0000);
        let routine = log_routine(Arch::X86_64, slots);
        assert_eq!(routine, slots * 16);
        for slot in [0u64, 1, 299] {
            let stub = &code[(slot * 16) as usize..][..16];
            // mov r11d, imm32.
            assert_eq!(&stub[..2], [0x41, 0xbb]);
            assert_eq!(le32(stub, 2) as u64, slot);
            // jmp rel32, from the end of the jump at 11.
            assert_eq!(stub[6], 0xe9);
            let rel = le32(stub, 7) as i32 as i64;
            assert_eq!(slot as i64 * 16 + 11 + rel, routine as i64, "slot {slot}");
            assert!(stub[11..].iter().all(|it| *it == 0xcc));
        }
    }

    #[test]
    fn the_x86_log_routine_marks_copies_and_jumps_saving_what_it_uses() {
        let (slots, p, header) = (7, 0x40_0000, 0x60_0010);
        let code = log_relocated(Arch::X86_64, slots, p, header);
        let at = log_routine(Arch::X86_64, slots) as usize;
        let routine = &code[at..];
        let p = p + at as u64;
        let rip = |disp: usize, end: u64| (p + end).wrapping_add(le32(routine, disp) as i32 as i64 as u64);
        // push rax; push rcx.
        assert_eq!(&routine[..2], [0x50, 0x51]);
        // mov rax, [rip + map].
        assert_eq!(&routine[2..5], [0x48, 0x8b, 0x05]);
        assert_eq!(rip(5, 9), header + exec::MAP);
        // mov byte [rax + r11 + 16], 1.
        assert_eq!(&routine[9..15], [0x42, 0xc6, 0x44, 0x18, 0x10, 0x01]);
        // mov rax, [rip + current]; mov rax, [rax + r11*8].
        assert_eq!(&routine[15..18], [0x48, 0x8b, 0x05]);
        assert_eq!(rip(18, 22), header + exec::CURRENT);
        assert_eq!(&routine[22..26], [0x4a, 0x8b, 0x04, 0xd8]);
        // lea rcx, [rip + log table]; mov [rcx + r11*8], rax.
        assert_eq!(&routine[26..29], [0x48, 0x8d, 0x0d]);
        assert_eq!(rip(29, 33), header + exec::log_word(0));
        assert_eq!(&routine[33..37], [0x4a, 0x89, 0x04, 0xd9]);
        // mov r11, rax; pop rcx; pop rax; jmp r11.
        assert_eq!(&routine[37..45], [0x49, 0x89, 0xc3, 0x59, 0x58, 0x41, 0xff, 0xe3]);
        assert_eq!(routine.len(), 48);
    }

    #[test]
    fn an_aarch64_log_stub_puts_its_slot_in_x16_and_branches_to_the_routine() {
        let slots = 0x1_0003;
        let code = log_relocated(Arch::Aarch64, slots, 0x10_0000, 0x80_0000);
        let routine = log_routine(Arch::Aarch64, slots);
        for slot in [0u64, 1, 0xffff, 0x1_0000, 0x1_0002] {
            let at = (slot * 16) as usize;
            let word = |n: usize| le32(&code, at + n * 4);
            assert_eq!(word(0) & !(0xffff << 5), 0xd280_0010, "movz x16");
            assert_eq!(word(1) & !(0xffff << 5), 0xf2a0_0010, "movk x16, lsl #16");
            assert_eq!(((word(0) >> 5) & 0xffff) as u64 | (((word(1) >> 5) & 0xffff) as u64) << 16, slot);
            assert_eq!(word(2) >> 26, 0b000101, "b");
            assert_eq!(at as u64 + 8 + u64::from(word(2) & 0x03ff_ffff) * 4, routine, "slot {slot}");
            assert_eq!(word(3), 0xd420_0000);
        }
    }

    #[test]
    fn the_aarch64_log_routine_marks_copies_and_branches_saving_x0() {
        let (slots, p, header) = (3, 0x10_0000, 0x7f_f018);
        let code = log_relocated(Arch::Aarch64, slots, p, header);
        let at = log_routine(Arch::Aarch64, slots) as usize;
        let p = p + at as u64;
        let word = |n: usize| le32(&code, at + n * 4);
        let lo12 = |n: usize| u64::from((word(n) >> 10) & 0xfff);
        // str x0, [sp, #-16]! ... ldr x0, [sp], #16; br x17.
        assert_eq!((word(0), word(12), word(13)), (0xf81f_0fe0, 0xf841_07e0, 0xd61f_0220));
        // The map: adrp x17 and an 8-byte load from its low 12 bits.
        assert_eq!(adrp(word(1), p + 4) + lo12(2) * 8, header + exec::MAP);
        assert_eq!(word(2) & !(0xfff << 10), 0xf940_0231);
        // add x17, x17, x16; movz w0, #1; strb w0, [x17, #16].
        assert_eq!((word(3), word(4), word(5)), (0x8b10_0231, 0x5280_0020, 0x3900_4220));
        // The current generation's table, then its word for the slot.
        assert_eq!(adrp(word(6), p + 24) + lo12(7) * 8, header + exec::CURRENT);
        assert_eq!(word(8), 0xf870_7a31);
        // The log table, by an add of its low 12 bits, then its word for the slot.
        assert_eq!(adrp(word(9), p + 36) + lo12(10), header + exec::log_word(0));
        assert_eq!(word(10) & !(0xfff << 10), 0x9100_0000);
        assert_eq!(word(11), 0xf830_7811);
        assert_eq!((word(14), word(15)), (0xd420_0000, 0xd420_0000));
    }

    #[test]
    fn the_bulk_log_stubs_are_each_slots_log_stub() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let slots = 9000u64;
            let mut all = vec![0; log_size(arch, slots) as usize];
            log_stubs(arch, slots, &mut all);
            let size = log_stub_size(arch);
            for slot in [0u32, 1, 4095, 4096, 8191, 8999] {
                let mut one = vec![0; size as usize];
                write_log_stub(arch, slot, log_routine(arch, slots) - u64::from(slot) * size, &mut one);
                assert_eq!(&all[(u64::from(slot) * size) as usize..][..size as usize], &one[..], "{arch:?} slot {slot}");
            }
        }
    }

    #[test]
    fn the_farthest_aarch64_log_stub_still_reaches_the_routine() {
        let mut one = [0; 16];
        let to = log_routine(Arch::Aarch64, A64_LOG_SLOTS);
        write_log_stub(Arch::Aarch64, 0, to, &mut one);
        let imm = le32(&one, 8) & 0x03ff_ffff;
        assert!(imm < 1 << 25, "a forward branch");
        assert_eq!(8 + u64::from(imm) * 4, to);
        assert!(log_size(Arch::Aarch64, A64_LOG_SLOTS) <= 1 << 27);
    }

    #[test]
    fn capacity_leaves_room_only_when_incremental() {
        assert_eq!(capacity(10, false), 10);
        assert_eq!(capacity(10, true), 10 + (1 << 16));
        assert_eq!(capacity(1 << 20, true), 2 << 20);
    }
}
