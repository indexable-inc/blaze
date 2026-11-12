//! The launcher's own machine code: a gate's entry, the lazy stubs and the resolver they
//! reach. Byte templates, made here and written by the control thread; nothing in this module
//! touches memory.
//!
//! # A gate (launcher.md, section 4)
//!
//! ```text
//! gate:   .quad flag                 ; 0 until the commit, then 1 (`Write::Commit`)
//! entry:  if [flag] != 0 { jump [new] } else { jump [old] }
//!         .quad old, new             ; absolute addresses
//! ```
//!
//! An entry reads the flag once and jumps: a call resolves old or new at that load. It
//! changes no register a call passes anything in: x16 on AArch64 (the register a call
//! through a word already holds its target in), none on x86-64.
//!
//! # The stubs (launcher.md, section 1)
//!
//! One region a process: a head ([`stub_head`]), then one stub a slot ([`stub`]). A slot's word names its stub until the
//! slot is bound. A stub says which slot it is by where it is (its own address), and jumps to
//! the head, which adds the launcher's context and jumps to [`resolver`]: that saves every
//! register an argument can be in, calls `table::resolve` (which asks the control thread for
//! the slot's code and waits), and jumps *through the slot's word again*. It is never handed
//! the address of a body, so a body can be replaced while a thread is still in the resolver.

/// The bytes a gate begins with: its flag.
pub const GATE_HEAD: u64 = 8;
/// The bytes of one entry of a gate, on both instruction sets.
pub const GATE_ENTRY: u64 = 40;
/// A gate's alignment: its flag's and its literals'.
pub const GATE_ALIGN: u64 = 8;
/// The bytes the stub region begins with.
pub const STUB_HEAD: usize = 32;
/// The bytes of one stub.
#[cfg(target_arch = "aarch64")]
pub const STUB: usize = 8;
/// The bytes of one stub.
#[cfg(target_arch = "x86_64")]
pub const STUB: usize = 16;

/// A branch or a load the instruction set cannot encode from `from` to `to`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutOfReach {
    pub what: &'static str,
    pub from: u64,
    pub to: u64,
}

impl core::fmt::Display for OutOfReach {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let OutOfReach { what, from, to } = *self;
        write!(f, "{what}: {to:#x} is out of reach of {from:#x}")
    }
}

impl core::error::Error for OutOfReach {}

#[cfg(target_arch = "aarch64")]
mod encode {
    /// `br x16`.
    pub(super) const BR_X16: u32 = 0xd61f_0200;
    /// `br x15`.
    pub(super) const BR_X15: u32 = 0xd61f_01e0;
    /// `adr x16, .`: x16 is this instruction's own address.
    pub(super) const ADR_X16_HERE: u32 = 0x1000_0010;

    /// `ldr x<reg>, <this instruction + delta>`: `delta` is a multiple of 4 within 1 MiB.
    pub(super) const fn ldr_literal(reg: u32, delta: i64) -> u32 {
        0x5800_0000 | ((((delta >> 2) as u32) & 0x7_ffff) << 5) | reg
    }

    /// `cbnz x<reg>, <this instruction + delta>`.
    pub(super) const fn cbnz(reg: u32, delta: i64) -> u32 {
        0xb500_0000 | ((((delta >> 2) as u32) & 0x7_ffff) << 5) | reg
    }

    /// `b <this instruction + delta>`: `delta` is a multiple of 4 within 128 MiB.
    pub(super) const fn b(delta: i64) -> u32 {
        0x1400_0000 | (((delta >> 2) as u32) & 0x03ff_ffff)
    }
}

/// The entry at address `at` of the gate whose flag is at `flag`: jump to `new` when the flag
/// is set, else to `old`.
#[cfg(target_arch = "aarch64")]
pub fn gate_entry(at: u64, flag: u64, old: u64, new: u64) -> Result<[u8; 40], OutOfReach> {
    use encode::{BR_X16, cbnz, ldr_literal};

    let back = flag.wrapping_sub(at) as i64;
    if back % 4 != 0 || !(-(1 << 20)..(1 << 20)).contains(&back) {
        return Err(OutOfReach { what: "a gate entry's load of its flag", from: at, to: flag });
    }
    let code = [
        ldr_literal(16, back), //  0: ldr  x16, flag
        cbnz(16, 12),          //  4: cbnz x16, 16
        ldr_literal(16, 16),   //  8: ldr  x16, old (at 24)
        BR_X16,                // 12: br   x16
        ldr_literal(16, 16),   // 16: ldr  x16, new (at 32)
        BR_X16,                // 20: br   x16
    ];
    let mut bytes = [0; 40];
    for (index, word) in code.iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    bytes[24..32].copy_from_slice(&old.to_le_bytes());
    bytes[32..40].copy_from_slice(&new.to_le_bytes());
    Ok(bytes)
}

/// The entry at address `at` of the gate whose flag is at `flag`: jump to `new` when the flag
/// is set, else to `old`.
#[cfg(target_arch = "x86_64")]
pub fn gate_entry(at: u64, flag: u64, old: u64, new: u64) -> Result<[u8; 40], OutOfReach> {
    // The compare is 8 bytes long, and its displacement counts from its end.
    let Ok(back) = i32::try_from(flag.wrapping_sub(at + 8) as i64) else {
        return Err(OutOfReach { what: "a gate entry's load of its flag", from: at, to: flag });
    };
    let mut bytes = [0xcc; 40];
    //  0: cmp qword ptr [rip + flag], 0
    bytes[0..3].copy_from_slice(&[0x48, 0x83, 0x3d]);
    bytes[3..7].copy_from_slice(&back.to_le_bytes());
    bytes[7] = 0;
    //  8: jne 16
    bytes[8..10].copy_from_slice(&[0x75, 0x06]);
    // 10: jmp qword ptr [rip + 8]   ; old, at 24
    bytes[10..16].copy_from_slice(&[0xff, 0x25, 8, 0, 0, 0]);
    // 16: jmp qword ptr [rip + 10]  ; new, at 32
    bytes[16..22].copy_from_slice(&[0xff, 0x25, 10, 0, 0, 0]);
    bytes[24..32].copy_from_slice(&old.to_le_bytes());
    bytes[32..40].copy_from_slice(&new.to_le_bytes());
    Ok(bytes)
}

/// The stub region's head: hands `context` to the code at `entry` ([`resolver`]).
#[cfg(target_arch = "aarch64")]
pub fn stub_head(context: u64, entry: u64) -> [u8; STUB_HEAD] {
    use encode::{BR_X15, ldr_literal};

    let code = [
        ldr_literal(17, 16), //  0: ldr x17, context (at 16)
        ldr_literal(15, 20), //  4: ldr x15, entry (at 24)
        BR_X15,              //  8: br  x15
        0,                   // 12: (never run)
    ];
    let mut bytes = [0; STUB_HEAD];
    for (index, word) in code.iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    bytes[16..24].copy_from_slice(&context.to_le_bytes());
    bytes[24..32].copy_from_slice(&entry.to_le_bytes());
    bytes
}

/// The stub of `slot`, which is [`STUB_HEAD`]` + slot * `[`STUB`] bytes after the head:
/// `adr x16, .; b head`.
#[cfg(target_arch = "aarch64")]
pub fn stub(slot: u32) -> Result<[u8; STUB], OutOfReach> {
    use encode::{ADR_X16_HERE, b};

    let at = STUB_HEAD as u64 + u64::from(slot) * STUB as u64;
    if at + 4 >= 1 << 27 {
        return Err(OutOfReach { what: "a stub's branch to the head", from: at, to: 0 });
    }
    let mut bytes = [0; STUB];
    bytes[0..4].copy_from_slice(&ADR_X16_HERE.to_le_bytes());
    bytes[4..8].copy_from_slice(&b(-(at as i64 + 4)).to_le_bytes());
    Ok(bytes)
}

/// The stub region's head: hands `context` to the code at `entry` ([`resolver`]).
#[cfg(target_arch = "x86_64")]
pub fn stub_head(context: u64, entry: u64) -> [u8; STUB_HEAD] {
    let mut bytes = [0xcc; STUB_HEAD];
    //  0: push qword ptr [rip + 10]  ; context, at 16
    bytes[0..6].copy_from_slice(&[0xff, 0x35, 10, 0, 0, 0]);
    //  6: jmp  qword ptr [rip + 12]  ; entry, at 24
    bytes[6..12].copy_from_slice(&[0xff, 0x25, 12, 0, 0, 0]);
    bytes[16..24].copy_from_slice(&context.to_le_bytes());
    bytes[24..32].copy_from_slice(&entry.to_le_bytes());
    bytes
}

/// The stub of `slot`, which is [`STUB_HEAD`]` + slot * `[`STUB`] bytes after the head:
/// `lea r11, [rip - 7]; jmp head`.
#[cfg(target_arch = "x86_64")]
pub fn stub(slot: u32) -> Result<[u8; STUB], OutOfReach> {
    let at = STUB_HEAD as u64 + u64::from(slot) * STUB as u64;
    // The jump's displacement counts from its end, 12 bytes into the stub.
    let Ok(back) = i32::try_from(at + 12) else {
        return Err(OutOfReach { what: "a stub's jump to the head", from: at, to: 0 });
    };
    let mut bytes = [0xcc; STUB];
    //  0: lea r11, [rip - 7]  ; r11 is this stub's own address
    bytes[0..7].copy_from_slice(&[0x4c, 0x8d, 0x1d, 0xf9, 0xff, 0xff, 0xff]);
    //  7: jmp head
    bytes[7] = 0xe9;
    bytes[8..12].copy_from_slice(&(-back).to_le_bytes());
    Ok(bytes)
}

/// Where a stub's jump arrives, through the head: x16 is the stub's address and x17 the
/// launcher's context. Every register an argument can be in is as the caller left it, and x30
/// is the caller's return address.
///
/// It keeps a frame record (x29, x30), so a walk of the stack passes through it; it has no
/// unwind entry yet (launcher.md, step 3), so a panic cannot unwind through a stub that is
/// resolving.
///
/// # Safety
///
/// Reached only by a jump from a stub ([`stub`], through [`stub_head`]), never called from Rust: it
/// does not follow the C convention on entry (its arguments are x16 and x17) or on exit (it
/// leaves by a jump through the slot's word).
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn resolver() {
    core::arch::naked_asm!(
        // x0 to x7 and x8 (the address of a returned aggregate), q0 to q7.
        "stp x29, x30, [sp, #-224]!",
        "mov x29, sp",
        "stp x0, x1, [sp, #16]",
        "stp x2, x3, [sp, #32]",
        "stp x4, x5, [sp, #48]",
        "stp x6, x7, [sp, #64]",
        "str x8, [sp, #80]",
        "stp q0, q1, [sp, #96]",
        "stp q2, q3, [sp, #128]",
        "stp q4, q5, [sp, #160]",
        "stp q6, q7, [sp, #192]",
        "mov x0, x16",
        "mov x1, x17",
        "bl {resolve}",
        // The address of the slot's word, kept where no argument is.
        "mov x17, x0",
        "ldp q6, q7, [sp, #192]",
        "ldp q4, q5, [sp, #160]",
        "ldp q2, q3, [sp, #128]",
        "ldp q0, q1, [sp, #96]",
        "ldr x8, [sp, #80]",
        "ldp x6, x7, [sp, #64]",
        "ldp x4, x5, [sp, #48]",
        "ldp x2, x3, [sp, #32]",
        "ldp x0, x1, [sp, #16]",
        "ldp x29, x30, [sp], #224",
        // The call again, as every call is made: load the word, jump.
        "ldr x16, [x17]",
        "br x16",
        resolve = sym crate::table::resolve,
    )
}

/// Where a stub's jump arrives, through the head: r11 is the stub's address, the launcher's
/// context is on the stack above the caller's return address. Every register an argument can
/// be in is as the caller left it.
///
/// It has no unwind entry yet (launcher.md, step 3), so a panic cannot unwind through a stub
/// that is resolving, and it keeps no frame record: a walk that meets it does not finish,
/// which holds everything (the core's rule for an incomplete walk).
///
/// # Safety
///
/// Reached only by a jump from a stub ([`stub`], through [`stub_head`]), never called from Rust: it
/// does not follow the C convention on entry (its arguments are r11 and a word on the stack)
/// or on exit (it leaves by a jump through the slot's word).
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub unsafe extern "C" fn resolver() {
    core::arch::naked_asm!(
        // The stack is 16-aligned here: the call pushed 8, the head pushed 8. Eight pushes
        // and 128 bytes keep it so for the call below. rax is the count of vector arguments
        // of a variadic call; r10 is the static chain.
        "push rdi",
        "push rsi",
        "push rdx",
        "push rcx",
        "push r8",
        "push r9",
        "push rax",
        "push r10",
        "sub rsp, 128",
        "movups xmmword ptr [rsp], xmm0",
        "movups xmmword ptr [rsp + 16], xmm1",
        "movups xmmword ptr [rsp + 32], xmm2",
        "movups xmmword ptr [rsp + 48], xmm3",
        "movups xmmword ptr [rsp + 64], xmm4",
        "movups xmmword ptr [rsp + 80], xmm5",
        "movups xmmword ptr [rsp + 96], xmm6",
        "movups xmmword ptr [rsp + 112], xmm7",
        "mov rdi, r11",
        // The context: above the 128 bytes and the eight pushes.
        "mov rsi, qword ptr [rsp + 192]",
        "call {resolve}",
        // The address of the slot's word, kept where no argument is.
        "mov r11, rax",
        "movups xmm7, xmmword ptr [rsp + 112]",
        "movups xmm6, xmmword ptr [rsp + 96]",
        "movups xmm5, xmmword ptr [rsp + 80]",
        "movups xmm4, xmmword ptr [rsp + 64]",
        "movups xmm3, xmmword ptr [rsp + 48]",
        "movups xmm2, xmmword ptr [rsp + 32]",
        "movups xmm1, xmmword ptr [rsp + 16]",
        "movups xmm0, xmmword ptr [rsp]",
        "add rsp, 128",
        "pop r10",
        "pop rax",
        "pop r9",
        "pop r8",
        "pop rcx",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        // The context goes; the caller's return address is on top again.
        "add rsp, 8",
        // The call again, as every call is made: jump through the word.
        "jmp qword ptr [r11]",
        resolve = sym crate::table::resolve,
    )
}
