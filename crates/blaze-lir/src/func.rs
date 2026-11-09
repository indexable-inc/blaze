//! A LIR function as columns: [`Func`] owns them in an allocator (the compile's frame while the
//! builder makes it), [`View`] borrows them, of a `Func` or of a record read in place
//! ([`crate::persist::read`]). Every reader takes a `View`, so a kept LIR is read with no
//! decoding into another structure.
//!
//! The columns are listed once ([`columns!`]) and `Func`, `View` and the record's root are
//! made from that list, so none can miss one.
//!
//! The tables' rows are blaze-format records, the same bytes in a `Func`'s `Vec` as in a
//! kept record. An index into a table that may name nothing is 0 for none and 1 + the row
//! otherwise (`attrs`, `locs`, a parameter's facts, a range); one that is a parent or an
//! optional place is [`NONE`] for none.

use std::{
    alloc::{Allocator, Global},
    fmt,
    ops::Range,
};

use blaze_format::SpanRef;
use blaze_ids::ContentId;

use crate::{
    op::{Op, Operands},
    ty::Ty,
};

/// "None" in a `u32` field that is a place, parent or row when it is not.
pub const NONE: u32 = u32::MAX;

macro_rules! ids {
    ($($(#[$meta:meta])* $name:ident = $prefix:literal;)*) => {$(
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
        #[repr(transparent)]
        pub struct $name(pub u32);

        impl $name {
            pub const fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }
    )*};
}

ids! {
    /// A value: the index of the instruction that makes it (a block parameter is an
    /// [`Op::Param`] instruction).
    Value = "v";
    /// A block, by its place in layout order.
    Block = "block";
    /// A row of `refs`: a function, static or thread-local by place, or the function's own data.
    RefId = "ref";
    /// A row of `sigs`.
    SigId = "s";
    /// A row of `slots`.
    SlotId = "ss";
    /// A row of `intrinsics`.
    IntrinsicId = "i";
    /// A row of `asms`.
    AsmId = "a";
    /// A row of `datas`.
    DataId = "d";
}

/// What a block is for: how a consumer reaches and lays it out.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum BlockKind {
    Normal,
    /// Rarely run (a panic path, `cold_path`): a hint, laid out apart.
    Cold,
    /// An unwind edge's target that runs drops and resumes: its one parameter is the exception.
    Cleanup,
    /// The `try` intrinsic's unwind edge: catches the exception, its one parameter.
    Catch,
    /// `UnwindAction::Terminate`: a cleanup that calls `panic_cannot_unwind`.
    Terminate,
}

impl BlockKind {
    pub const ALL: [BlockKind; 5] = [
        BlockKind::Normal,
        BlockKind::Cold,
        BlockKind::Cleanup,
        BlockKind::Catch,
        BlockKind::Terminate,
    ];

    pub const fn from_byte(byte: u8) -> Option<BlockKind> {
        match (byte as usize) < BlockKind::ALL.len() {
            true => Some(BlockKind::ALL[byte as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            BlockKind::Normal => "normal",
            BlockKind::Cold => "cold",
            BlockKind::Cleanup => "cleanup",
            BlockKind::Catch => "catch",
            BlockKind::Terminate => "terminate",
        }
    }

    /// Whether an unwind edge leads here.
    pub const fn is_landing(self) -> bool {
        matches!(self, BlockKind::Cleanup | BlockKind::Catch | BlockKind::Terminate)
    }
}

blaze_format::record! {
    /// What a function is besides its columns.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct Head: 1 {
        /// The target's pointer width in bits, 32 or 64: the width of `ptr_addr`'s integer.
        pub ptr_bits: u32,
        /// The function's own signature, a row of `sigs`.
        pub sig: u32,
        /// Its attributes ([`FnAttrs`]).
        pub attrs: u32,
    }
}

/// A function's attributes, as rustc gives LLVM them: `-> !`, nounwind (panic=abort, an
/// `extern "C"` without `-unwind`), `#[cold]`, `#[inline]`'s three forms, `#[optimize]`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[repr(transparent)]
pub struct FnAttrs(pub u32);

/// `#[inline]`, as a function attribute: a hint (`inlinehint`), `always`, `never`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Inline {
    None = 0,
    Hint = 1,
    Always = 2,
    Never = 3,
}

impl FnAttrs {
    pub const NORETURN: u32 = 1 << 0;
    pub const NOUNWIND: u32 = 1 << 1;
    pub const COLD: u32 = 1 << 2;
    /// Bits 3..5: an [`Inline`].
    pub const INLINE_SHIFT: u32 = 3;
    pub const OPT_SIZE: u32 = 1 << 5;
    pub const OPT_NONE: u32 = 1 << 6;
    pub const ALL: u32 = (1 << 7) - 1;

    /// The single-bit attributes, by their text names.
    pub const NAMED: [(u32, &'static str); 5] = [
        (FnAttrs::NORETURN, "noreturn"),
        (FnAttrs::NOUNWIND, "nounwind"),
        (FnAttrs::COLD, "cold"),
        (FnAttrs::OPT_SIZE, "optsize"),
        (FnAttrs::OPT_NONE, "optnone"),
    ];

    pub const fn inline(self) -> Inline {
        match (self.0 >> FnAttrs::INLINE_SHIFT) & 0x3 {
            1 => Inline::Hint,
            2 => Inline::Always,
            3 => Inline::Never,
            _ => Inline::None,
        }
    }

    pub const fn with_inline(self, inline: Inline) -> FnAttrs {
        FnAttrs((self.0 & !(0x3 << FnAttrs::INLINE_SHIFT)) | ((inline as u32) << FnAttrs::INLINE_SHIFT))
    }
}

impl Inline {
    pub const fn name(self) -> &'static str {
        match self {
            Inline::None => "",
            Inline::Hint => "inlinehint",
            Inline::Always => "alwaysinline",
            Inline::Never => "noinline",
        }
    }
}

blaze_format::record! {
    /// A compound type's members: `compound_tys[first..first + len]`.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct CompoundRec: 1 {
        pub first: u32,
        pub len: u32,
    }
}

/// A calling convention. A Rust-ABI function uses its target's C convention over its lowered
/// signature, so stencil code, LLVM code and C call each other with no adapter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Conv {
    /// The target's C convention: System V, Win64, AAPCS64 or Apple's.
    C,
    /// `extern "sysv64"`.
    SysV64,
    /// `extern "win64"`.
    Win64,
    /// `extern "aapcs"`.
    Aapcs,
    /// WebAssembly's.
    Wasm,
    /// `extern "rust-cold"`: LLVM's `preserve_mostcc`.
    PreserveMost,
}

impl Conv {
    pub const ALL: [Conv; 6] =
        [Conv::C, Conv::SysV64, Conv::Win64, Conv::Aapcs, Conv::Wasm, Conv::PreserveMost];

    pub const fn from_byte(byte: u8) -> Option<Conv> {
        match (byte as usize) < Conv::ALL.len() {
            true => Some(Conv::ALL[byte as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Conv::C => "c",
            Conv::SysV64 => "sysv64",
            Conv::Win64 => "win64",
            Conv::Aapcs => "aapcs",
            Conv::Wasm => "wasm",
            Conv::PreserveMost => "preserve_most",
        }
    }

    pub fn from_name(name: &str) -> Option<Conv> {
        Conv::ALL.into_iter().find(|it| it.name() == name)
    }
}

blaze_format::record! {
    /// A lowered signature: `sig_params[first..first + params]` its parameters, then
    /// `rets` returns after them.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct SigRec: 1 {
        pub first: u32,
        pub params: u16,
        pub rets: u16,
        /// A [`Conv`].
        pub conv: u8,
        /// [`SigRec::VARIADIC`].
        pub bits: u8,
        pub pad: [u8; 2],
    }
}

impl SigRec {
    /// C's `...`: the call passes more arguments than the parameters.
    pub const VARIADIC: u8 = 1 << 0;
}

/// How an integer parameter or return is widened to its register.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Ext {
    None,
    Zext,
    Sext,
}

impl Ext {
    pub const ALL: [Ext; 3] = [Ext::None, Ext::Zext, Ext::Sext];

    pub const fn from_byte(byte: u8) -> Option<Ext> {
        match (byte as usize) < Ext::ALL.len() {
            true => Some(Ext::ALL[byte as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Ext::None => "",
            Ext::Zext => "zext",
            Ext::Sext => "sext",
        }
    }
}

/// How a parameter is passed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum ParamKind {
    /// In its type's registers, or on the stack past them.
    Value,
    /// The pointer to where the return goes (`PassMode::Indirect` for a return): `sret`.
    Sret,
    /// A C aggregate copied onto the stack (`PassMode::OnStack`): `byval(size, align)`.
    Byval,
}

impl ParamKind {
    pub const ALL: [ParamKind; 3] = [ParamKind::Value, ParamKind::Sret, ParamKind::Byval];

    pub const fn from_byte(byte: u8) -> Option<ParamKind> {
        match (byte as usize) < ParamKind::ALL.len() {
            true => Some(ParamKind::ALL[byte as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            ParamKind::Value => "",
            ParamKind::Sret => "sret",
            ParamKind::Byval => "byval",
        }
    }
}

blaze_format::record! {
    /// A parameter or return of a signature.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct ParamRec: 1 {
        /// 0, or 1 + its row of `param_facts`.
        pub facts: u32,
        pub ty: u16,
        /// An [`Ext`].
        pub ext: u8,
        /// A [`ParamKind`].
        pub kind: u8,
    }
}

blaze_format::record! {
    /// What rustc tells LLVM of a parameter or return (`arg_attrs_for_rust_scalar`,
    /// `rustc_codegen_llvm::abi`), each a promise.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct ParamFactsRec: 1 {
        /// `dereferenceable(n)` (`dereferenceable_or_null(n)` with [`ParamFactsRec::DEREF_OR_NULL`]);
        /// 0 for none.
        pub deref: u64,
        /// The pointee's size of a `byval` or `sret` parameter, 0 for another.
        pub size: u32,
        /// 0, or 1 + a row of `ranges`: the scalar's valid range (`range`).
        pub range: u32,
        /// The log2 of the pointee's alignment (`align`), 0 for none: a `byval`'s stack
        /// alignment, a reference's or an `sret`'s pointee's.
        pub align: u8,
        pub bits: u8,
        pub pad: [u8; 6],
    }
}

impl ParamFactsRec {
    pub const NONNULL: u8 = 1 << 0;
    pub const NOALIAS: u8 = 1 << 1;
    pub const READONLY: u8 = 1 << 2;
    pub const NOUNDEF: u8 = 1 << 3;
    pub const CAPTURES_NONE: u8 = 1 << 4;
    pub const DEREF_OR_NULL: u8 = 1 << 5;
    pub const WRITABLE: u8 = 1 << 6;
    pub const DEAD_ON_UNWIND: u8 = 1 << 7;

    pub const NAMED: [(u8, &'static str); 8] = [
        (ParamFactsRec::NONNULL, "nonnull"),
        (ParamFactsRec::NOALIAS, "noalias"),
        (ParamFactsRec::READONLY, "readonly"),
        (ParamFactsRec::NOUNDEF, "noundef"),
        (ParamFactsRec::CAPTURES_NONE, "captures_none"),
        (ParamFactsRec::DEREF_OR_NULL, "deref_or_null"),
        (ParamFactsRec::WRITABLE, "writable"),
        (ParamFactsRec::DEAD_ON_UNWIND, "dead_on_unwind"),
    ];
}

blaze_format::record! {
    /// What a load, store, atomic or bulk memory operation is told of its memory
    /// (`rustc_codegen_llvm`'s load metadata and access alignment), each a promise: a row of
    /// `mem_attrs`, named by the instruction's `attrs` word. An access with none is of
    /// alignment 1 and claims nothing.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct MemAttrsRec: 1 {
        /// 0, or 1 + a row of `ranges`: the loaded scalar's valid range (`!range`).
        pub range: u32,
        /// The log2 of the access's alignment (`align n`); for a `memcpy`/`memmove`, the
        /// destination's.
        pub align: u8,
        /// The log2 of a `memcpy`/`memmove`'s source alignment, 0 for another operation.
        pub src_align: u8,
        /// The log2 of a loaded pointer's pointee alignment (`!align`), 0 for none.
        pub pointee_align: u8,
        pub bits: u8,
    }
}

impl MemAttrsRec {
    /// A loaded pointer is not null (`!nonnull`).
    pub const NONNULL: u8 = 1 << 0;
    /// The loaded bytes are initialized (`!noundef`).
    pub const NOUNDEF: u8 = 1 << 1;
    /// The memory never changes while it is reachable (a vtable slot: `!invariant.load`).
    pub const INVARIANT: u8 = 1 << 2;

    pub const NAMED: [(u8, &'static str); 3] = [
        (MemAttrsRec::NONNULL, "nonnull"),
        (MemAttrsRec::NOUNDEF, "noundef"),
        (MemAttrsRec::INVARIANT, "invariant"),
    ];
}

blaze_format::record! {
    /// A scalar's valid range, as rustc's `WrappingRange`: `start..=end`, wrapping past the
    /// type's maximum when `start > end`, in the type's width.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct RangeRec: 1 {
        pub start: u128,
        pub end: u128,
    }
}

/// What a row of `refs` refers to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum RefKind {
    /// A function, by its place in the instance's references (`Described::refs`).
    Func,
    /// A static, by place.
    Static,
    /// A thread-local, by place.
    Tls,
    /// The function's own data (a constant, a `file!()` text, a panic location): `place` is
    /// its row of `datas`.
    Data,
}

impl RefKind {
    pub const ALL: [RefKind; 4] = [RefKind::Func, RefKind::Static, RefKind::Tls, RefKind::Data];

    pub const fn from_word(word: u32) -> Option<RefKind> {
        match (word as usize) < RefKind::ALL.len() {
            true => Some(RefKind::ALL[word as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            RefKind::Func => "func",
            RefKind::Static => "static",
            RefKind::Tls => "tls",
            RefKind::Data => "data",
        }
    }

    pub fn from_name(name: &str) -> Option<RefKind> {
        RefKind::ALL.into_iter().find(|it| it.name() == name)
    }
}

blaze_format::record! {
    /// Something the function refers to, by place, never by identity: two instances whose
    /// code differs only in what they call are equal LIR.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct RefRec: 1 {
        pub place: u32,
        /// A [`RefKind`].
        pub kind: u32,
    }
}

blaze_format::record! {
    /// Data of the function's own, by content: `data_bytes[bytes_first..][..bytes_len]`,
    /// aligned to `1 << align`, with `data_relocs[relocs_first..][..relocs_len]` the addresses
    /// in it.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct DataRec: 1 {
        pub bytes_first: u32,
        pub bytes_len: u32,
        pub relocs_first: u32,
        pub relocs_len: u32,
        pub align: u32,
    }
}

blaze_format::record! {
    /// An address in data: at byte `offset`, the address of `refs[target]` plus `addend`.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct DataRelocRec: 1 {
        pub addend: i64,
        pub offset: u32,
        pub target: u32,
    }
}

blaze_format::record! {
    /// A target intrinsic (`core::arch`'s `link_name = "llvm.*"`): its name in `names` and
    /// its signature, a row of `sigs`.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct IntrinsicRec: 1 {
        pub name_first: u32,
        pub name_len: u32,
        pub sig: u32,
    }
}

/// What a stack slot holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum SlotKind {
    /// A temporary.
    Temp,
    /// A named local, which debug info tells of.
    Named,
    /// An argument passed on the stack, spilled.
    ArgSpill,
}

impl SlotKind {
    pub const ALL: [SlotKind; 3] = [SlotKind::Temp, SlotKind::Named, SlotKind::ArgSpill];

    pub const fn from_byte(byte: u8) -> Option<SlotKind> {
        match (byte as usize) < SlotKind::ALL.len() {
            true => Some(SlotKind::ALL[byte as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            SlotKind::Temp => "temp",
            SlotKind::Named => "named",
            SlotKind::ArgSpill => "arg_spill",
        }
    }

    pub fn from_name(name: &str) -> Option<SlotKind> {
        SlotKind::ALL.into_iter().find(|it| it.name() == name)
    }
}

blaze_format::record! {
    /// A stack slot: `size` bytes aligned to `1 << align` (any power of two).
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct SlotRec: 1 {
        pub size: u32,
        pub align: u8,
        /// A [`SlotKind`].
        pub kind: u8,
        pub pad: [u8; 2],
    }
}

blaze_format::record! {
    /// An `asm!` block: its template's pieces, its operands, its options and the ABIs it
    /// clobbers (`clobber_abi`, comma-separated in `asm_text`).
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct AsmRec: 1 {
        pub pieces_first: u32,
        pub pieces_len: u32,
        pub operands_first: u32,
        pub operands_len: u32,
        pub options: u32,
        pub clobbers_first: u32,
        pub clobbers_len: u32,
    }
}

impl AsmRec {
    pub const PURE: u32 = 1 << 0;
    pub const NOMEM: u32 = 1 << 1;
    pub const READONLY: u32 = 1 << 2;
    pub const PRESERVES_FLAGS: u32 = 1 << 3;
    pub const NORETURN: u32 = 1 << 4;
    pub const NOSTACK: u32 = 1 << 5;
    pub const ATT_SYNTAX: u32 = 1 << 6;
    pub const RAW: u32 = 1 << 7;
    pub const MAY_UNWIND: u32 = 1 << 8;

    pub const NAMED: [(u32, &'static str); 9] = [
        (AsmRec::PURE, "pure"),
        (AsmRec::NOMEM, "nomem"),
        (AsmRec::READONLY, "readonly"),
        (AsmRec::PRESERVES_FLAGS, "preserves_flags"),
        (AsmRec::NORETURN, "noreturn"),
        (AsmRec::NOSTACK, "nostack"),
        (AsmRec::ATT_SYNTAX, "att_syntax"),
        (AsmRec::RAW, "raw"),
        (AsmRec::MAY_UNWIND, "may_unwind"),
    ];
}

blaze_format::record! {
    /// A piece of an `asm!` template: text (`kind` 0, `asm_text[a..][..b]`), or an operand
    /// (`kind` 1, operand `a` of the block, `b` its modifier character or 0).
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct AsmPieceRec: 1 {
        pub kind: u32,
        pub a: u32,
        pub b: u32,
    }
}

impl AsmPieceRec {
    pub const TEXT: u32 = 0;
    pub const OPERAND: u32 = 1;
}

/// What an `asm!` operand is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum AsmOperandKind {
    In,
    Out,
    InOut,
    LateOut,
    InLateOut,
    /// `sym`: `aux` is a row of `refs`.
    Sym,
    /// `label`: `aux` is its place among the `asm_br`'s labels.
    Label,
}

impl AsmOperandKind {
    pub const ALL: [AsmOperandKind; 7] = [
        AsmOperandKind::In,
        AsmOperandKind::Out,
        AsmOperandKind::InOut,
        AsmOperandKind::LateOut,
        AsmOperandKind::InLateOut,
        AsmOperandKind::Sym,
        AsmOperandKind::Label,
    ];

    pub const fn from_byte(byte: u8) -> Option<AsmOperandKind> {
        match (byte as usize) < AsmOperandKind::ALL.len() {
            true => Some(AsmOperandKind::ALL[byte as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            AsmOperandKind::In => "in",
            AsmOperandKind::Out => "out",
            AsmOperandKind::InOut => "inout",
            AsmOperandKind::LateOut => "lateout",
            AsmOperandKind::InLateOut => "inlateout",
            AsmOperandKind::Sym => "sym",
            AsmOperandKind::Label => "label",
        }
    }

    pub fn from_name(name: &str) -> Option<AsmOperandKind> {
        AsmOperandKind::ALL.into_iter().find(|it| it.name() == name)
    }

    /// Whether the operand takes a value in: one of the `asm`'s arguments, in operand order.
    pub const fn takes(self) -> bool {
        matches!(self, AsmOperandKind::In | AsmOperandKind::InOut | AsmOperandKind::InLateOut)
    }

    /// Whether the operand gives a value out: a member of the `asm`'s result, in operand
    /// order.
    pub const fn gives(self) -> bool {
        matches!(
            self,
            AsmOperandKind::Out
                | AsmOperandKind::InOut
                | AsmOperandKind::LateOut
                | AsmOperandKind::InLateOut
        )
    }
}

blaze_format::record! {
    /// An `asm!` operand: its register or register class (`asm_text[reg_first..][..reg_len]`, empty
    /// for `sym` and `label`), and `aux` as its kind says.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct AsmOperandRec: 1 {
        pub reg_first: u32,
        pub reg_len: u32,
        pub aux: u32,
        /// An [`AsmOperandKind`].
        pub kind: u8,
        /// 0 an explicit register (`"rax"`), 1 a class (`reg`).
        pub class: u8,
        pub pad: [u8; 2],
    }
}

blaze_format::record! {
    /// A named local, for debug info: today's `side::Variable`.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct VarRec: 1 {
        /// Its binding, by index in the store of the body it is written in.
        pub binding: u32,
        /// Its slot, a row of `slots`.
        pub slot: u32,
        /// [`NONE`], or how far from the address the slot holds the local is.
        pub pointer: u32,
        /// [`NONE`] (all of the function, as a parameter's), or its row of `scope_parents`.
        pub scope: u32,
        /// 1 for a parameter, 0 otherwise.
        pub param: u32,
        /// Its type by content.
        pub ty: ContentId,
    }
}

blaze_format::record! {
    /// A body the MIR optimizer inlined into the function, inside `parent` (a row of
    /// `inlined`, [`NONE`] for the function's own), called at `call_site`.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct InlinedRec: 1 {
        pub parent: u32,
        pub call_site: SpanRef,
    }
}

/// A span's scope (`span_scopes`): of the scope of the code before it, of none, or of a row
/// of `scope_parents`.
pub const SCOPE_BEFORE: u32 = 0;
pub const SCOPE_OUTSIDE: u32 = 1;
/// `span_scopes`' word for scope row `n` is `SCOPE_IN + n`.
pub const SCOPE_IN: u32 = 2;

/// Calls `$m!` with every column of a function, in the order the record keeps them: its name,
/// its item type and what it is.
macro_rules! columns {
    ($m:ident) => {
        $m! {
            /// Per instruction: its [`Op`].
            ops: u8,
            /// Per instruction: its [`crate::Flags`].
            flags: u8,
            /// Per instruction: its result's [`Ty`], [`Ty::NONE`] for none.
            tys: u16,
            /// Per instruction: its one word, as [`Op::info`]'s layout says.
            data: u32,
            /// Operand lists, wide immediates, block targets with their arguments.
            extra: u32,
            /// Per instruction, cold: 0, or 1 + its row of `mem_attrs`.
            attrs: u32,
            /// Per instruction, cold: 0, or 1 + its row of `spans`.
            locs: u32,
            /// Per block, in layout order: its first instruction.
            block_starts: u32,
            /// Per block: its [`BlockKind`].
            block_kinds: u8,
            /// The compound types' members, [`CompoundRec`] rows over `compound_tys`.
            compounds: CompoundRec,
            compound_tys: u16,
            /// Signatures: the function's own and every callee's.
            sigs: SigRec,
            sig_params: ParamRec,
            param_facts: ParamFactsRec,
            /// Memory attributes, named by `attrs`.
            mem_attrs: MemAttrsRec,
            /// Valid ranges, named by memory attributes and parameter facts.
            ranges: RangeRec,
            /// What the function refers to.
            refs: RefRec,
            /// The function's own data.
            datas: DataRec,
            data_bytes: u8,
            data_relocs: DataRelocRec,
            /// Target intrinsics.
            intrinsics: IntrinsicRec,
            /// Stack slots.
            slots: SlotRec,
            /// `asm!` blocks.
            asms: AsmRec,
            asm_pieces: AsmPieceRec,
            asm_operands: AsmOperandRec,
            /// The intrinsics' names, in row order.
            names: u8,
            /// The asm blocks' text (template pieces, registers, clobbers), in row order.
            asm_text: u8,
            /// The span table `locs` names: position-free spans.
            spans: SpanRef,
            /// Empty, or per span its scope (`SCOPE_*`).
            span_scopes: u32,
            /// Empty, or per span the innermost row of `inlined` its code is of, [`NONE`] for
            /// the function's own.
            span_inlined: u32,
            /// Named locals.
            variables: VarRec,
            /// Per scope, the scope it is inside of ([`NONE`] for none), which comes before it.
            scope_parents: u32,
            /// Inlined bodies.
            inlined: InlinedRec,
            /// The function's target features (`#[target_feature]`), as LLVM's
            /// `"target-features"`: `+avx2,+fma`.
            features: u8,
        }
    };
}

macro_rules! define {
    ($($(#[$meta:meta])* $name:ident: $ty:ty,)*) => {
        /// A LIR function, its columns in the allocator `A`.
        #[derive(Clone, Debug)]
        pub struct Func<A: Allocator = Global> {
            pub head: Head,
            $( $(#[$meta])* pub $name: Vec<$ty, A>, )*
        }

        /// A LIR function's columns, borrowed: of a [`Func`] ([`Func::view`]) or of a record
        /// read in place ([`crate::persist::read`]).
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub struct View<'a> {
            pub head: Head,
            $( $(#[$meta])* pub $name: &'a [$ty], )*
        }

        impl<A: Allocator + Clone> Func<A> {
            /// An empty function in `alloc`.
            pub fn new_in(head: Head, alloc: A) -> Func<A> {
                Func { head, $( $name: Vec::new_in(alloc.clone()), )* }
            }
        }

        impl<A: Allocator> Func<A> {
            pub fn view(&self) -> View<'_> {
                View { head: self.head, $( $name: &self.$name, )* }
            }
        }

        impl View<'_> {
            /// A copy of the columns in `alloc`.
            pub fn to_func_in<A: Allocator + Clone>(&self, alloc: A) -> Func<A> {
                let mut func = Func::new_in(self.head, alloc);
                $( func.$name.extend_from_slice(self.$name); )*
                func
            }

            /// The columns' names and lengths, in record order: what a reader of the record
            /// checks the lengths of, and what a size report prints.
            pub fn lengths(&self) -> [(&'static str, usize); COLUMNS] {
                [$( (stringify!($name), self.$name.len()), )*]
            }
        }

        /// How many columns a function has.
        pub const COLUMNS: usize = [$( stringify!($name), )*].len();
    };
}

pub(crate) use columns;

columns!(define);

/// The hot bytes per instruction: an op, flags, a type and one word. data.md's budget.
const _: () = assert!(size_of::<u8>() + size_of::<u8>() + size_of::<u16>() + size_of::<u32>() == 8);
const _: () = assert!(size_of::<SigRec>() == 12);
const _: () = assert!(size_of::<ParamRec>() == 8);
const _: () = assert!(size_of::<MemAttrsRec>() == 8);
const _: () = assert!(size_of::<VarRec>() == 52);

/// A signature, read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sig<'a> {
    pub conv: u8,
    pub bits: u8,
    pub params: &'a [ParamRec],
    pub rets: &'a [ParamRec],
}

impl<'a> View<'a> {
    /// How many instructions (and so value numbers) the function has.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Instruction `inst`'s operation. Panics on a byte that names none, which a built or a
    /// read function never has.
    pub fn op(&self, inst: usize) -> Op {
        Op::from_byte(self.ops[inst]).expect("an instruction of a checked function")
    }

    pub fn flags(&self, inst: usize) -> crate::Flags {
        crate::Flags(self.flags[inst])
    }

    /// The type of `value`, the result of its instruction.
    pub fn ty(&self, value: Value) -> Ty {
        Ty(self.tys[value.index()])
    }

    /// Instruction `inst`'s operands, as its layout says.
    pub fn operands(&self, inst: usize) -> Operands<'a> {
        Operands::of(*self, inst)
    }

    pub fn blocks(&self) -> usize {
        self.block_starts.len()
    }

    /// The instructions of `block`.
    pub fn block(&self, block: Block) -> Range<usize> {
        let start = self.block_starts[block.index()] as usize;
        let end = self.block_starts.get(block.index() + 1).map_or(self.len(), |&it| it as usize);
        start..end
    }

    pub fn block_kind(&self, block: Block) -> BlockKind {
        BlockKind::from_byte(self.block_kinds[block.index()]).expect("a block of a checked function")
    }

    /// The block's parameters: its leading `param` instructions.
    pub fn params(&self, block: Block) -> Range<usize> {
        let range = self.block(block);
        let end = (range.start..range.end)
            .find(|&inst| self.ops[inst] != Op::Param as u8)
            .unwrap_or(range.end);
        range.start..end
    }

    /// The block's last instruction, its terminator.
    pub fn terminator(&self, block: Block) -> usize {
        self.block(block).end - 1
    }

    /// The members of compound type `ty`.
    pub fn members(&self, ty: Ty) -> &'a [u16] {
        let Some(row) = ty.compound_row() else { return &[] };
        let rec = self.compounds[row as usize];
        &self.compound_tys[rec.first as usize..][..rec.len as usize]
    }

    pub fn sig(&self, sig: SigId) -> Sig<'a> {
        let rec = self.sigs[sig.index()];
        let params = &self.sig_params[rec.first as usize..][..rec.params as usize];
        let rets =
            &self.sig_params[rec.first as usize + rec.params as usize..][..rec.rets as usize];
        Sig { conv: rec.conv, bits: rec.bits, params, rets }
    }

    /// The facts of a parameter or return, if it has any.
    pub fn facts(&self, param: &ParamRec) -> Option<&'a ParamFactsRec> {
        param.facts.checked_sub(1).map(|row| &self.param_facts[row as usize])
    }

    /// Instruction `inst`'s memory attributes, if it has any.
    pub fn mem_attrs(&self, inst: usize) -> Option<&'a MemAttrsRec> {
        self.attrs.get(inst)?.checked_sub(1).map(|row| &self.mem_attrs[row as usize])
    }

    /// A range field (0, or 1 + a row).
    pub fn range(&self, field: u32) -> Option<&'a RangeRec> {
        field.checked_sub(1).map(|row| &self.ranges[row as usize])
    }

    /// Instruction `inst`'s source location, if it has one.
    pub fn loc(&self, inst: usize) -> Option<SpanRef> {
        self.locs.get(inst)?.checked_sub(1).map(|row| self.spans[row as usize])
    }

    /// `asm_text[first..first + len]`.
    pub fn asm_str(&self, first: u32, len: u32) -> &'a [u8] {
        &self.asm_text[first as usize..][..len as usize]
    }

    pub fn reference(&self, id: RefId) -> RefRec {
        self.refs[id.index()]
    }

    /// The bytes of data row `id`.
    pub fn data_bytes_of(&self, id: DataId) -> &'a [u8] {
        let rec = self.datas[id.index()];
        &self.data_bytes[rec.bytes_first as usize..][..rec.bytes_len as usize]
    }

    /// The addresses in data row `id`.
    pub fn data_relocs_of(&self, id: DataId) -> &'a [DataRelocRec] {
        let rec = self.datas[id.index()];
        &self.data_relocs[rec.relocs_first as usize..][..rec.relocs_len as usize]
    }

    pub fn intrinsic_name(&self, id: IntrinsicId) -> &'a [u8] {
        let rec = self.intrinsics[id.index()];
        &self.names[rec.name_first as usize..][..rec.name_len as usize]
    }

    pub fn asm_pieces_of(&self, id: AsmId) -> &'a [AsmPieceRec] {
        let rec = self.asms[id.index()];
        &self.asm_pieces[rec.pieces_first as usize..][..rec.pieces_len as usize]
    }

    pub fn asm_operands_of(&self, id: AsmId) -> &'a [AsmOperandRec] {
        let rec = self.asms[id.index()];
        &self.asm_operands[rec.operands_first as usize..][..rec.operands_len as usize]
    }
}
