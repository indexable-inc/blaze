//! The stubs of a session's lazy functions: one shared resolver and one 16-byte entry per
//! function, generated as bytes for the run process's code memory.
//!
//! A call reaches a lazy function through its slot: blaze-link's stub loads the slot and
//! jumps there, which is the function's entry until its code is published. The entry hands
//! the function's [`LazyId`] to the resolver in a register no call carries a value in (on
//! x86-64 pushed on the stack, as glibc's PLT stubs push the relocation index; on AArch64 in
//! `x16`, AAPCS64's intra-procedure-call register). The resolver:
//!
//! 1. saves every register a caller can have put a value in that the callee reads, or expects
//!    kept: on x86-64 `rax` (`al` carries the vector-register count of a variadic call),
//!    `rcx`, `rdx`, `rsi`, `rdi`, `r8` to `r10` and, with `XSAVEC`/`XSAVE`, the whole vector
//!    state the OS enabled (`xmm`, `ymm`, `zmm`, mask and APX registers, as glibc's
//!    `_dl_runtime_resolve_xsavec`), `FXSAVE` where there is no `XSAVE`; on AArch64 `x0` to
//!    `x15` (`x8` the indirect result register, `x9` to `x15` for callers that keep values
//!    there across calls, as `preserve_most` does) and `q0` to `q31` whole, or with SVE every
//!    `z` and `p` register and `FFR`;
//! 2. calls the run process's hook (its address in the region's first word) with the id, on
//!    a stack aligned as the C ABI wants; the hook asks the daemon and returns the address
//!    the daemon published in the slot, or never returns;
//! 3. restores everything and tail-jumps there, the stack exactly as the caller left it, so
//!    stack arguments, the return address and the return registers are the callee's as if it
//!    had been called directly. A hook that returns 0 traps (`ud2`, `brk`): the resolver
//!    never jumps anywhere else.
//!
//! Only `r11` (x86-64) and `x16`, `x17` (AArch64) are not kept: blaze-link's slot stubs
//! clobber them on every call already, and no ABI passes anything in them.
//!
//! The region is position independent (every reference in it is relative), so it is
//! generated once and written anywhere in code memory. Its layout ([`Layout`]): the hook's
//! cell at 0, the resolver at [`RESOLVER`], then the entries, [`ENTRY`] bytes apart.

use std::fmt;

use crate::{LazyId, MOST_LAZY};

pub(crate) mod aarch64;
mod cpu;
mod x86_64;

/// The size of one entry.
pub const ENTRY: u32 = 16;
/// Where the hook's address is: the word the resolver calls through.
pub const CELL: u32 = 0;
/// Where the resolver starts.
pub const RESOLVER: u32 = 64;
/// The most bytes the stack grows by between two probes: under the smallest guard page.
const PROBE: u32 = 2048;

/// What the resolver is generated for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Isa {
    X86_64 { abi: Abi, save: X86Save },
    Aarch64 { sve: bool },
}

/// The calling convention the hook is called with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Abi {
    /// Linux and macOS: the id in `edi`.
    SysV,
    /// Windows: the id in `ecx`, with 32 bytes of shadow space.
    Win64,
}

/// How the x86-64 resolver saves the vector and x87 state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum X86Save {
    /// `FXSAVE64`: x87, MXCSR and `xmm0` to `xmm15`, on a processor without `XSAVE` (and so
    /// without any wider state).
    Fx,
    /// `XSAVE64` of the components in `mask`, in the standard format of `size` bytes.
    Xsave { mask: u64, size: u32 },
    /// `XSAVEC64` of the components in `mask`, compacted to `size` bytes.
    Xsavec { mask: u64, size: u32 },
}

impl X86Save {
    /// The bytes of the save area, a multiple of 64.
    pub fn size(self) -> u32 {
        match self {
            X86Save::Fx => 512,
            X86Save::Xsave { size, .. } | X86Save::Xsavec { size, .. } => size.next_multiple_of(64),
        }
    }
}

impl Isa {
    /// The machine this process runs on, which is the run process's: the daemon runs it on
    /// the same machine.
    pub fn host() -> Result<Isa, StubError> {
        #[cfg(target_arch = "x86_64")]
        {
            let abi = if cfg!(windows) { Abi::Win64 } else { Abi::SysV };
            Ok(Isa::X86_64 { abi, save: cpu::x86_save() })
        }
        #[cfg(target_arch = "aarch64")]
        {
            Ok(Isa::Aarch64 { sve: std::arch::is_aarch64_feature_detected!("sve") })
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Err(StubError::Platform(std::env::consts::ARCH))
        }
    }
}

/// Why a region could not be generated.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StubError {
    /// Lazy stubs exist for x86-64 and AArch64 only.
    Platform(&'static str),
    /// More entries than one region's branches reach.
    TooMany { count: u32, most: u32 },
    /// A save area the resolver can't address in one instruction.
    Save(u32),
}

impl fmt::Display for StubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StubError::Platform(arch) => {
                write!(f, "lazy compilation's stubs are for x86-64 and AArch64, and this is {arch}")
            }
            StubError::TooMany { count, most } => {
                write!(f, "{count} lazy functions in one stub region, which reaches {most}")
            }
            StubError::Save(size) => write!(f, "an XSAVE area of {size} bytes"),
        }
    }
}

impl std::error::Error for StubError {}

/// Where everything is in a region, as offsets from its start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    pub entries: u32,
    pub count: u32,
    pub len: u32,
}

impl Layout {
    /// The offset of `id`'s entry.
    pub fn entry(&self, id: LazyId) -> u32 {
        debug_assert!(id.0 < self.count);
        self.entries + id.0 * ENTRY
    }

    /// The function whose entry starts at `offset`, if one does.
    pub fn id_at(&self, offset: u32) -> Option<LazyId> {
        let from = offset.checked_sub(self.entries)?;
        (from % ENTRY == 0 && from / ENTRY < self.count).then_some(LazyId(from / ENTRY))
    }
}

/// A region's bytes and layout.
#[derive(PartialEq, Eq, Debug)]
pub struct Stubs {
    pub bytes: Vec<u8>,
    pub layout: Layout,
}

/// The most entries a region has on `isa`: an entry's branch to the resolver reaches 128 MiB
/// on AArch64 and 2 GiB on x86-64, and an id stays below [`MOST_LAZY`].
pub fn most(isa: &Isa) -> u32 {
    let reach: u64 = match isa {
        Isa::X86_64 { .. } => 1 << 31,
        Isa::Aarch64 { .. } => 1 << 27,
    };
    let entries = (reach - u64::from(RESOLVER) - 4096) / u64::from(ENTRY);
    entries.min(u64::from(MOST_LAZY - 1)) as u32
}

/// The region of `count` entries for `isa`, its cell holding `hook`, the address of the run
/// process's hook (0 to write it later, at [`CELL`]). One pass over a buffer of the exact
/// size: each entry is a copy of one template with its id and branch filled in.
pub fn generate(isa: &Isa, count: u32, hook: u64) -> Result<Stubs, StubError> {
    let most = most(isa);
    if count > most {
        return Err(StubError::TooMany { count, most });
    }
    let mut bytes = Vec::with_capacity(RESOLVER as usize + 2048 + (count * ENTRY) as usize);
    bytes.extend_from_slice(&hook.to_le_bytes());
    let fill: u8 = match isa {
        // `int3`, and `udf #0` four bytes at a time: padding traps.
        Isa::X86_64 { .. } => 0xcc,
        Isa::Aarch64 { .. } => 0,
    };
    bytes.resize(RESOLVER as usize, fill);
    match *isa {
        Isa::X86_64 { abi, save } => x86_64::resolver(&mut bytes, abi, save)?,
        Isa::Aarch64 { sve } => aarch64::resolver(&mut bytes, sve),
    }
    let entries = (bytes.len() as u32).next_multiple_of(ENTRY);
    bytes.resize(entries as usize, fill);
    let len = entries + count * ENTRY;
    bytes.resize(len as usize, 0);
    let template = match isa {
        Isa::X86_64 { .. } => x86_64::ENTRY_TEMPLATE,
        Isa::Aarch64 { .. } => aarch64::ENTRY_TEMPLATE,
    };
    for (id, entry) in bytes[entries as usize..].chunks_exact_mut(ENTRY as usize).enumerate() {
        let at = entries + id as u32 * ENTRY;
        entry.copy_from_slice(&template);
        match isa {
            Isa::X86_64 { .. } => x86_64::fill_entry(entry, at, id as u32),
            Isa::Aarch64 { .. } => aarch64::fill_entry(entry, at, id as u32),
        }
    }
    Ok(Stubs { bytes, layout: Layout { entries, count, len } })
}
