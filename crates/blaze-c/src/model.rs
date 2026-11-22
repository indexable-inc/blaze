//! The declaration model: what a C header gives a Rust program, as columns (docs/v2/plan/design/data.md:
//! a tag and a few words a row, lists as ranges into one flat column, names by number).
//!
//! A front end fills a [`DeclsBuilder`] in any order it likes; [`crate::canon::encode`] writes
//! the one canonical record from it. Every reference is to something made before it (a type to
//! earlier types, a field to a type), except that a record or an enum is DECLARED before it is
//! DEFINED ([`DeclsBuilder::declare_record`], [`DeclsBuilder::define_record`]), which is how C's
//! `struct node { struct node *next; }` is written. So the type graph has no cycle but through a
//! declaration, by construction.
//!
//! Sizes, alignments and offsets are the FRONT END's, for the target: the model computes no
//! layout. A construct the model cannot hold is a [`Unexpressed`] row (a typed cause and where
//! it came from), or an [`CType::Unexpressible`] type for a type inside a declaration: never a
//! dropped declaration.
//!
//! What the model holds for C++ (D176) and C leaves empty: a namespace per declaration
//! ([`NsId`]), overload sets (several function rows of one name, each with its own linker
//! symbol), methods (a function row with an owner record and a [`MethodKind`]), base classes,
//! a record's [`Relocation`], reference types ([`CType::Reference`]) and a function's
//! [`FnAttrs::MAY_THROW`].

use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc, Vec};

use crate::place::{RelPath, Root};
use crate::{NONE, at, narrow};

/// A name, by number into the model's (or the record's) name table. [`NONE`] is no name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Name(pub u32);

/// A type of the type column.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TypeId(pub u32);

/// A record (struct or union) declaration.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RecordId(pub u32);

/// An enum declaration.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EnumId(pub u32);

/// A typedef declaration.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TypedefId(pub u32);

/// A header the front end read, by number into the model's header table.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct HeaderId(pub u32);

/// A variable declaration, by number into the model's variable column (what a
/// [`MacroValue::Slot`] or [`MacroValue::AddressOf`] names).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct VarId(pub u32);

/// A namespace (C++); [`NsId::GLOBAL`] is the global one, every C declaration's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NsId(pub u32);

impl NsId {
    pub const GLOBAL: NsId = NsId(NONE);
}

/// Where a declaration came from: its header, where the declaration starts in it (bytes from the
/// header's start) and its length. A field's or an enum constant's place is relative to its
/// declaration's start (rule 71: item-relative); nothing holds a line or a column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Origin {
    pub header: HeaderId,
    pub at: u32,
    pub len: u32,
}

macro_rules! byte_enum {
    ($(#[$m:meta])* pub enum $name:ident { $($(#[$vm:meta])* $v:ident = $n:literal,)* }) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub enum $name { $($(#[$vm])* $v = $n,)* }
        impl $name {
            /// Every value, in byte order.
            pub const ALL: &'static [$name] = &[$($name::$v,)*];
            /// The value's byte, as the record holds it.
            pub const fn byte(self) -> u8 { self as u8 }
            /// The value of `byte`; `None` for a byte no value has.
            pub const fn from_byte(byte: u8) -> Option<$name> {
                match byte { $($n => Some($name::$v),)* _ => None }
            }
        }
    };
}
pub(crate) use byte_enum;

byte_enum! {
    /// C's integer types (the character types among them), each with the target's size.
    pub enum IntKind {
        Char = 0,
        SChar = 1,
        UChar = 2,
        Short = 3,
        UShort = 4,
        Int = 5,
        UInt = 6,
        Long = 7,
        ULong = 8,
        LongLong = 9,
        ULongLong = 10,
        Int128 = 11,
        UInt128 = 12,
    }
}

byte_enum! {
    /// C's floating types.
    pub enum FloatKind {
        Float = 0,
        Double = 1,
        LongDouble = 2,
        Float16 = 3,
        Float128 = 4,
    }
}

byte_enum! {
    /// A function type's calling convention, as the front end resolved it for the target
    /// (`__attribute__((ms_abi))` on x86-64 is [`Conv::Win64`]). One a Rust ABI string cannot
    /// name (`regparm`, `preserve_most`) is an [`CType::Unexpressible`] type with
    /// [`Unexpressed::CallingConvention`].
    pub enum Conv {
        C = 0,
        Win64 = 1,
        SysV64 = 2,
        Stdcall = 3,
        Fastcall = 4,
        Thiscall = 5,
        Vectorcall = 6,
        Aapcs = 7,
    }
}

byte_enum! {
    /// A reference's value category (C++).
    pub enum ValueCategory {
        LValue = 0,
        RValue = 1,
    }
}

byte_enum! {
    pub enum RecordKind {
        Struct = 0,
        Union = 1,
    }
}

byte_enum! {
    /// What moving a value of a record type means (C++, D176). Every C type is
    /// [`Relocation::TriviallyCopyable`].
    pub enum Relocation {
        TriviallyCopyable = 0,
        TriviallyRelocatable = 1,
        AddressOnly = 2,
    }
}

byte_enum! {
    /// What a function row is: a free function (every C function), or a C++ member.
    pub enum MethodKind {
        Free = 0,
        Static = 1,
        InstanceConst = 2,
        InstanceMut = 3,
        Constructor = 4,
        Destructor = 5,
    }
}

byte_enum! {
    /// What kind of declaration a row or a finding is about.
    pub enum DeclKind {
        Function = 0,
        Variable = 1,
        Typedef = 2,
        Record = 3,
        Enum = 4,
        Constant = 5,
        Macro = 6,
        Field = 7,
        Other = 8,
    }
}

byte_enum! {
    /// Why a declaration, or a type inside one, is not expressed in Rust: each a finding at the
    /// import (rule 81), never a silent skip (rule 3). Causes 0 to 15 and 37 to 39 are the front
    /// end's (it saw a construct the model has no column for); 16 to 29 and 40 up are the
    /// rendering's (the model holds it, Rust as rendered cannot); 30 to 36 are C++ parts the
    /// model holds and the rendering does not render yet.
    ///
    /// A function-like macro is one of three causes, so the count of what was left out says what
    /// the import must learn next (docs/v2/plan/design/c.md, rung C4): [`Unexpressed::FunctionLikeMacro`]
    /// could be a function, [`Unexpressed::MacroIsFragment`] and
    /// [`Unexpressed::MacroPastesOrAssigns`] never can.
    pub enum Unexpressed {
        /// `#define PyArray_ISCONTIGUOUS(m) PyArray_CHKFLAGS((m), NPY_ARRAY_C_CONTIGUOUS)`: a
        /// function-like macro whose expansion is a typed expression of its arguments. It has no
        /// Rust declaration of its own; a `macro_fn` in the import asks for a function of it by
        /// signature ([`crate::grammar::MacroFn`]).
        FunctionLikeMacro = 0,
        /// An object-like macro whose expansion is not a constant, nor a table slot or a
        /// variable's address ([`MacroValue::Slot`], [`MacroValue::AddressOf`]): a type, a call,
        /// an operator over a variable.
        MacroNotConstant = 1,
        /// A function the headers define with internal or inline linkage ([`Linkage::Internal`],
        /// [`Linkage::Inline`]) that is not rendered: the import asked for no bodies
        /// (`inline_bodies off;`), the headers read declare it and never define it, or it is
        /// variadic (the thunk the import's unit writes for it cannot pass `...` on).
        InlineFunction = 2,
        /// A C++ construct in a C import (a class, a template, `extern "C++"`).
        CppConstruct = 3,
        /// `_Atomic T`.
        Atomic = 4,
        /// `_Complex T`.
        Complex = 5,
        /// A vector type (`__attribute__((vector_size))`, `ext_vector_type`).
        Vector = 6,
        /// `_BitInt(N)`.
        BitInt = 7,
        /// `_Thread_local` data.
        ThreadLocal = 8,
        /// A function with no prototype (`int f();` before C23).
        Unprototyped = 9,
        /// A calling convention or ABI attribute Rust has no ABI string for (`regparm`).
        CallingConvention = 10,
        /// A bitfield the model cannot hold (wider than its storage unit, of a non-integer type).
        Bitfield = 11,
        /// A wide or Unicode string macro (`L"..."`, `u8"..."` of another char type).
        WideString = 12,
        /// `__attribute__((cleanup))`, `overloadable` and other attributes that change what a
        /// call means.
        Attribute = 13,
        /// A declaration the front end could not lower for another reason it names by its
        /// diagnostic code (a [`crate::front::FrontFault::Diagnostic`]).
        FrontEnd = 14,
        /// A function-like or object-like macro whose expansion is not an expression: a
        /// declaration (`#define PyObject_HEAD PyObject ob_base;`, cpython object.h:60), a
        /// statement (`#define Py_RETURN_NONE return Py_None`, object.h:1118), an initializer
        /// list (`PyObject_HEAD_INIT`, object.h:135) or an unbalanced block
        /// (`Py_TRASHCAN_BEGIN`, cpython/object.h:489). No function or constant can hold it.
        MacroIsFragment = 15,
        /// `long double` of more than eight bytes, by value: Rust has no such type.
        LongDouble = 16,
        /// `_Float16` by value (Rust's `f16` is unstable).
        Float16 = 17,
        /// `_Float128` by value (Rust's `f128` is unstable).
        Float128 = 18,
        /// A variadic function of another convention than C's.
        VariadicConvention = 19,
        /// `vectorcall` (unstable in Rust).
        UnstableConvention = 20,
        /// A record whose layout the rendering cannot reproduce with `repr(C)`, `packed` and
        /// `align` (packed and over-aligned at once, a field placed below its alignment): it is
        /// rendered as sized bytes with no fields.
        LayoutNotReproduced = 21,
        /// Two declarations render to one Rust name in one namespace; the later in the kind order
        /// (records, enums, typedefs, functions, variables, constants) is dropped.
        NameClash = 22,
        /// Uses, by value, a declaration that was itself dropped or rendered opaque.
        DependsOnDropped = 23,
        /// Passes by value a record whose rendering is not ABI-exact (padding the rendering
        /// inserted, bytes in place of a field): the call would classify it differently.
        ByValueNotExact = 24,
        /// Uses by value a record or enum that was only declared.
        IncompleteByValue = 25,
        /// A string macro with an interior NUL: no `CStr` literal holds it.
        StringWithNul = 26,
        /// `returns_twice` (`setjmp`): Rust cannot call it soundly.
        ReturnsTwice = 27,
        /// A typedef of `void`, or another type that is not a value.
        NotAValue = 28,
        /// A bitfield whose bits span more than the sixteen bytes an accessor reads.
        BitfieldWindow = 29,
        /// A declaration in a C++ namespace (the rendering makes no nested modules yet).
        CppNamespace = 30,
        /// An overload set of more than one member.
        CppOverloads = 31,
        /// A method, constructor or destructor.
        CppMethod = 32,
        /// A record with base classes.
        CppBases = 33,
        /// A record that is not trivially copyable.
        CppRelocation = 34,
        /// A reference type.
        CppReference = 35,
        /// A function that may throw.
        CppMayThrow = 36,
        /// A function-like macro that pastes tokens (`Py##cls##ArrType_Type` in
        /// `PyArray_IsScalar`, numpy ndarrayobject.h:40) or assigns to an argument (`(out)=..`
        /// in `PyArray_HasArrayInterfaceType`, ndarrayobject.h:28): its arguments are not values,
        /// so no function of them has its meaning.
        MacroPastesOrAssigns = 37,
        /// A name Rust cannot spell even as a raw identifier (`self`, `Self`, `super`, `crate`,
        /// `_`).
        ReservedName = 40,
        /// Left out by the import's `allow` or `block` lists: a finding too, so a dropped
        /// declaration is always visible.
        NotSelected = 41,
        /// A function type outside a pointer (a typedef of a function type): Rust has no such
        /// type; uses through a pointer render as `Option<unsafe extern fn>`.
        FunctionTypedef = 42,
        /// A variable the headers define with internal linkage (`static void **PyArray_API =
        /// NULL;`, numpy __multiarray_api.h:644) where the rendering has no unit of the import's
        /// to hold it in (see [`crate::key::UnitKey`]).
        HeaderVariable = 43,
    }
}

byte_enum! {
    /// A function's or variable's linkage, as C gives it.
    ///
    /// | linkage | C | where the code or data is |
    /// |---|---|---|
    /// | external | a plain or `extern` declaration; GNU `extern inline` (its body is for inlining only) | the library's symbol |
    /// | internal | `static`, `static inline` (cpython object.h:316 `Py_REFCNT`; numpy ndarraytypes.h:1594 `PyArray_NDIM`) | each translation unit's own copy: the import's unit |
    /// | inline | C99's `inline` without `extern` or `static`: an inline definition, whose external one the program need not have | the import's unit |
    ///
    /// A variable is external or internal (`static void **PyArray_API = NULL;`, numpy
    /// __multiarray_api.h:644); `inline` is refused for one ([`ModelError::BadLinkage`]).
    pub enum Linkage {
        External = 0,
        Internal = 1,
        Inline = 2,
    }
}

impl Linkage {
    /// Whether the declaration's code or data is the import's own (its unit's), not the
    /// library's: each translation unit, and so each import, has its own.
    pub const fn is_local(self) -> bool {
        !matches!(self, Linkage::External)
    }
}

/// A variable's initializer, as the header gives it. The model holds it so two front ends are
/// compared on it and a reader sees it; the import's unit compiles the header's own text, so the
/// rendering never needs the value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum VarInit {
    /// A declaration, no definition: `extern void **PyArray_API;`.
    Absent,
    /// Zero-initialized: `= NULL`, `= 0`, `= {0}`, or a tentative definition (`static int n;`),
    /// which C zero-initializes.
    Zero,
    /// An integer constant initializer, its bits as for [`ConstRow::value`].
    Int(u64),
    /// An initializer the model holds no value for (an address, an aggregate list).
    Other,
}

/// The bytes of a [`VarRow`]'s `init` column.
pub(crate) mod init {
    pub(crate) const ABSENT: u8 = 0;
    pub(crate) const ZERO: u8 = 1;
    pub(crate) const INT: u8 = 2;
    pub(crate) const OTHER: u8 = 3;
}

/// A type's qualifiers, as a bit set.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Quals(pub u8);

impl Quals {
    pub const NONE: Quals = Quals(0);
    pub const CONST: Quals = Quals(1);
    pub const VOLATILE: Quals = Quals(2);
    /// `restrict` on a pointer: a promise, kept in the model, which changes no ABI and has no
    /// Rust spelling, so the rendering writes nothing for it.
    pub const RESTRICT: Quals = Quals(4);
    const ALL: u8 = 7;

    pub const fn has(self, q: Quals) -> bool {
        self.0 & q.0 == q.0
    }

    pub const fn with(self, q: Quals) -> Quals {
        Quals(self.0 | q.0)
    }
}

/// A function declaration's attributes, as a bit set.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FnAttrs(pub u32);

impl FnAttrs {
    pub const NONE: FnAttrs = FnAttrs(0);
    /// `_Noreturn`, `__attribute__((noreturn))`: rendered `-> !`.
    pub const NORETURN: FnAttrs = FnAttrs(1);
    /// `returns_twice`: not rendered ([`Unexpressed::ReturnsTwice`]).
    pub const RETURNS_TWICE: FnAttrs = FnAttrs(2);
    /// `deprecated`: rendered `#[deprecated]`.
    pub const DEPRECATED: FnAttrs = FnAttrs(4);
    /// C++: the function may throw (never set for C).
    pub const MAY_THROW: FnAttrs = FnAttrs(8);
    /// The symbol is literal (an asm label, `__asm("_f$UNIX2003")`): the platform's prefix is
    /// not added to it.
    pub const SYMBOL_LITERAL: FnAttrs = FnAttrs(16);
    const ALL: u32 = 31;

    pub const fn has(self, a: FnAttrs) -> bool {
        self.0 & a.0 == a.0
    }
}

/// A variable declaration's attributes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct VarAttrs(pub u32);

impl VarAttrs {
    pub const NONE: VarAttrs = VarAttrs(0);
    /// `_Thread_local`: not rendered ([`Unexpressed::ThreadLocal`]).
    pub const THREAD_LOCAL: VarAttrs = VarAttrs(1);
    /// As [`FnAttrs::SYMBOL_LITERAL`].
    pub const SYMBOL_LITERAL: VarAttrs = VarAttrs(16);
    const ALL: u32 = 17;

    pub const fn has(self, a: VarAttrs) -> bool {
        self.0 & a.0 == a.0
    }
}

/// A C type as a front end gives it to [`DeclsBuilder::ty`]. Scalars carry their size and
/// alignment in bytes AS THE FRONT END COMPUTED THEM for the target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CType<'l> {
    Void { quals: Quals },
    /// `_Bool`.
    Bool { size: u32, align: u32, quals: Quals },
    Int { kind: IntKind, size: u32, align: u32, signed: bool, quals: Quals },
    Float { kind: FloatKind, size: u32, align: u32, quals: Quals },
    /// A pointer; `quals` are the pointer's own (`restrict`, a `const` pointer), the pointee's
    /// are on the pointee type.
    Pointer { pointee: TypeId, quals: Quals },
    /// A C++ reference: a pointer that is never null, of a value category (D176).
    Reference { pointee: TypeId, category: ValueCategory },
    /// `len: None` is an array of unknown size (a flexible array member, `extern int t[];`).
    Array { elem: TypeId, len: Option<u64>, quals: Quals },
    /// A function type: its result, its parameters (after C's adjustments: an array or a
    /// function parameter is the pointer it adjusts to), variadic, prototyped, convention.
    Function { result: TypeId, params: &'l [TypeId], variadic: bool, prototyped: bool, conv: Conv },
    Record { record: RecordId, quals: Quals },
    Enum { decl: EnumId, quals: Quals },
    Typedef { decl: TypedefId, quals: Quals },
    /// `va_list`: the builtin, with the target's own type for it (a pointer on Apple arm64, an
    /// array of one `__va_list_tag` on x86-64, a struct on Linux arm64), which is what renders.
    VaList { lowered: TypeId, quals: Quals },
    /// A type the model holds no column for, by its cause, with its size and alignment where the
    /// front end knows them (0 where not), so a field of it keeps its record's layout.
    Unexpressible { cause: Unexpressed, size: u32, align: u32 },
}

/// The tags of the type column.
pub(crate) mod tag {
    pub(crate) const VOID: u8 = 0;
    pub(crate) const BOOL: u8 = 1;
    pub(crate) const INT: u8 = 2;
    pub(crate) const FLOAT: u8 = 3;
    pub(crate) const POINTER: u8 = 4;
    pub(crate) const ARRAY: u8 = 5;
    pub(crate) const FUNCTION: u8 = 6;
    pub(crate) const RECORD: u8 = 7;
    pub(crate) const ENUM: u8 = 8;
    pub(crate) const TYPEDEF: u8 = 9;
    pub(crate) const VA_LIST: u8 = 10;
    pub(crate) const UNEXPRESSIBLE: u8 = 11;
    pub(crate) const REFERENCE: u8 = 12;
    pub(crate) const LAST: u8 = 12;

    /// Flags of an `INT` row.
    pub(crate) const SIGNED: u8 = 1;
    /// Flags of an `ARRAY` row.
    pub(crate) const INCOMPLETE: u8 = 1;
    /// Flags of a `FUNCTION` row.
    pub(crate) const VARIADIC: u8 = 1;
    pub(crate) const PROTOTYPED: u8 = 2;
}

/// The kinds of a macro row's value.
pub(crate) mod value {
    pub(crate) const INT: u8 = 0;
    pub(crate) const FLOAT: u8 = 1;
    pub(crate) const STR: u8 = 2;
    /// [`super::MacroValue::Slot`].
    pub(crate) const SLOT: u8 = 3;
    /// [`super::MacroValue::AddressOf`].
    pub(crate) const ADDRESS_OF: u8 = 4;
    pub(crate) const LAST: u8 = 4;
}

blaze_format_core::record! {
    /// One type: a tag and three words, their meaning by tag (`tag` module):
    ///
    /// | tag | `a` | `b` | `c` | `extra` | `flags` |
    /// |---|---|---|---|---|---|
    /// | void | | | | | |
    /// | bool | size | align | | | |
    /// | int | size | align | | [`IntKind`] | signed |
    /// | float | size | align | | [`FloatKind`] | |
    /// | pointer | pointee | | | | |
    /// | reference | pointee | | | [`ValueCategory`] | |
    /// | array | element | length's low word | high word | | unknown length |
    /// | function | result | first parameter in `type_lists` | parameter count | [`Conv`] | variadic, prototyped |
    /// | record, enum, typedef | the declaration | | | | |
    /// | va_list | the target's type | | | | |
    /// | unexpressible | size | align | | [`Unexpressed`] | |
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
    pub struct TypeRow: 1 {
        pub a: u32,
        pub b: u32,
        pub c: u32,
        pub kind: u8,
        pub quals: u8,
        pub flags: u8,
        pub extra: u8,
    }
}

blaze_format_core::record! {
    /// A header a declaration came from: the digest of its root and relative path
    /// ([`crate::place::path_digest`]), and the root's byte (which says whether it is the
    /// package's).
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
    pub struct HeaderRow: 1 {
        pub path: ContentId,
        pub root: u8,
        pub pad: [u8; 3],
    }
}

blaze_format_core::record! {
    /// A namespace (C++): its name and its parent ([`NONE`] for one in the global namespace).
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct NamespaceRow: 1 {
        pub name: u32,
        pub parent: u32,
    }
}

blaze_format_core::record! {
    /// A struct or union: its size and alignment as the front end laid it out, `pack` the
    /// maximum field alignment `#pragma pack` or `packed` set (0 for none), its fields, its
    /// bitfields' storage units, its bases and its relocation fact (C++). `complete` false is a
    /// declaration with no definition (an opaque handle). `name` is [`NONE`] for an anonymous one.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct RecordRow: 1 {
        pub size: u64,
        pub name: u32,
        pub ns: u32,
        pub fields_start: u32,
        pub fields_len: u32,
        pub units_start: u32,
        pub units_len: u32,
        pub bases_start: u32,
        pub bases_len: u32,
        pub align: u32,
        pub pack: u32,
        pub header: u32,
        pub at: u32,
        pub len: u32,
        pub kind: u8,
        pub complete: u8,
        pub relocation: u8,
        pub pad: u8,
    }
}

blaze_format_core::record! {
    /// A field: its offset in BITS from the record's start, as the front end laid it out (for a
    /// bitfield, in the target's bit order: from the least significant bit of byte 0 on a
    /// little-endian target, the most significant on a big-endian one), its type, and for a
    /// bitfield its width and storage unit. `name` [`NONE`] is an anonymous member (C11) or an
    /// unnamed bitfield. `at` and `len` are relative to the record declaration's start.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct FieldRow: 1 {
        pub offset_bits: u64,
        pub name: u32,
        pub ty: u32,
        pub width: u32,
        pub unit: u32,
        pub at: u32,
        pub len: u32,
        pub bitfield: u8,
        pub pad: [u8; 7],
    }
}

blaze_format_core::record! {
    /// A bitfield storage unit: its offset and size in bytes, as the front end allocated it.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct UnitRow: 1 {
        pub offset: u32,
        pub size: u32,
    }
}

blaze_format_core::record! {
    /// A base class (C++): its record, its offset in bytes, `virtual` in `flags` bit 0.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct BaseRow: 1 {
        pub offset: u64,
        pub record: u32,
        pub flags: u8,
        pub pad: [u8; 3],
    }
}

blaze_format_core::record! {
    /// An enum: its underlying integer type and its constants in declaration order; `scoped`
    /// is C++'s `enum class`.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct EnumRow: 1 {
        pub name: u32,
        pub ns: u32,
        pub underlying: u32,
        pub consts_start: u32,
        pub consts_len: u32,
        pub header: u32,
        pub at: u32,
        pub len: u32,
        pub complete: u8,
        pub scoped: u8,
        pub pad: [u8; 2],
    }
}

blaze_format_core::record! {
    /// An enum constant: its value's bits (two's complement, sign-extended to 64 bits for a
    /// signed underlying type), its name, its place relative to the enum's start.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct ConstRow: 1 {
        pub value: u64,
        pub name: u32,
        pub at: u32,
        pub len: u32,
        pub pad: [u8; 4],
    }
}

blaze_format_core::record! {
    /// A function: one member of the overload set `name` in `ns` (C's sets have one), its linker
    /// symbol (the front end's: blaze never mangles), its function type, its owner record and
    /// [`MethodKind`] (C++; [`NONE`] and free for C), its parameter names (`params_start`,
    /// `params_len` into `name_lists`, [`NONE`] for an unnamed one) and [`FnAttrs`].
    ///
    /// `linkage` is a [`Linkage`]; `body` is 1 when the headers define the function (its body is
    /// in a header: the row's origin is the DEFINITION's header and range), 0 when they only
    /// declare it. Nothing of the code made from a body is here: the import's unit is keyed
    /// apart ([`crate::key::UnitKey`]), so this record, and its key, are the same before and
    /// after the body is compiled.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct FnRow: 1 {
        pub name: u32,
        pub ns: u32,
        pub symbol: u32,
        pub ty: u32,
        pub owner: u32,
        pub params_start: u32,
        pub params_len: u32,
        pub attrs: u32,
        pub header: u32,
        pub at: u32,
        pub len: u32,
        pub method: u8,
        pub linkage: u8,
        pub body: u8,
        pub pad: u8,
    }
}

blaze_format_core::record! {
    /// A global variable: its type (top-level `const` makes it immutable), its symbol,
    /// [`VarAttrs`], its [`Linkage`] (external: the library's data; internal: a header-local
    /// variable, one per translation unit) and its initializer (`init` a byte of the `init`
    /// module, `init_bits` the integer's bits for `INT`, else 0).
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct VarRow: 1 {
        pub init_bits: u64,
        pub name: u32,
        pub ns: u32,
        pub symbol: u32,
        pub ty: u32,
        pub attrs: u32,
        pub header: u32,
        pub at: u32,
        pub len: u32,
        pub linkage: u8,
        pub init: u8,
        pub pad: [u8; 6],
    }
}

blaze_format_core::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct TypedefRow: 1 {
        pub name: u32,
        pub ns: u32,
        pub ty: u32,
        pub header: u32,
        pub at: u32,
        pub len: u32,
    }
}

blaze_format_core::record! {
    /// An object-like macro that is a constant, with its C type: an integer (`value` its bits as
    /// for [`ConstRow`]), a float (`value` the bits of the `f64`, or of the `f32` in the low word
    /// for a type of four bytes) or a narrow string (`str_start`, `str_len` into `strings`, no
    /// NUL added); or an object-like macro that names a declaration (`kind` from the `value`
    /// module):
    ///
    /// | kind | `var` | `value` | `ty` | `deref` |
    /// |---|---|---|---|---|
    /// | slot | the table variable | the index | the pointer type the entry is cast to | 1 when the macro reads through the cast pointer |
    /// | address of | the variable | 0 | the variable's type | 0 |
    ///
    /// `var` is [`NONE`] for a constant.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct MacroRow: 1 {
        pub value: u64,
        pub name: u32,
        pub ty: u32,
        pub var: u32,
        pub str_start: u32,
        pub str_len: u32,
        pub header: u32,
        pub at: u32,
        pub len: u32,
        pub kind: u8,
        pub deref: u8,
        pub pad: [u8; 6],
    }
}

blaze_format_core::record! {
    /// A declaration the front end saw and the model cannot hold: its name, its kind, the
    /// [`Unexpressed`] cause and where.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct UnexpressedRow: 1 {
        pub name: u32,
        pub ns: u32,
        pub header: u32,
        pub at: u32,
        pub len: u32,
        pub kind: u8,
        pub cause: u8,
        pub pad: [u8; 2],
    }
}

blaze_format_core::record! {
    /// The target facts the rendering needs beyond the type rows: the pointer's size and
    /// alignment, the byte order (1 little-endian) and whether plain `char` is signed.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct AbiRow: 1 {
        pub pointer_size: u8,
        pub pointer_align: u8,
        pub little_endian: u8,
        pub char_signed: u8,
    }
}

/// A field as a front end gives it to [`DeclsBuilder::define_record`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Field {
    pub name: Option<Name>,
    pub ty: TypeId,
    pub offset_bits: u64,
    /// For a bitfield: its width in bits and its storage unit, by index into the units given
    /// with it.
    pub bitfield: Option<Bitfield>,
    /// Relative to the record declaration's start.
    pub at: u32,
    pub len: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bitfield {
    pub width: u32,
    pub unit: u32,
}

/// A record's definition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecordBody<'l> {
    pub size: u64,
    pub align: u32,
    /// `#pragma pack(n)` or `packed` (1): the most a field is aligned; `None` for neither.
    pub pack: Option<u32>,
    pub fields: &'l [Field],
    /// Storage units: (offset, size) in bytes.
    pub units: &'l [(u32, u32)],
    /// Bases (C++): (record, offset in bytes, virtual). Empty for C.
    pub bases: &'l [(RecordId, u64, bool)],
    pub relocation: Relocation,
}

/// An enum constant as a front end gives it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EnumConst {
    pub name: Name,
    pub value: u64,
    pub at: u32,
    pub len: u32,
}

/// A function as a front end gives it to [`DeclsBuilder::function`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Function<'l> {
    pub name: Name,
    pub ns: NsId,
    /// The linker symbol: the name itself for C, the asm label where there is one, the mangled
    /// name for C++.
    pub symbol: Name,
    /// A [`CType::Function`].
    pub ty: TypeId,
    pub param_names: &'l [Option<Name>],
    pub attrs: FnAttrs,
    /// The owner and kind of a C++ member; `None` for a free function.
    pub owner: Option<(RecordId, MethodKind)>,
    pub linkage: Linkage,
    /// The headers define the function: `origin` is then the definition's. A bodiless
    /// declaration of the same function (a `static inline` forward declaration, numpy
    /// ndarraytypes.h:1344, whose body is further on) may be given too: the record holds the one
    /// row (see [`crate::canon::encode`]).
    pub body: bool,
    pub origin: Origin,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Variable {
    pub name: Name,
    pub ns: NsId,
    pub symbol: Name,
    pub ty: TypeId,
    pub attrs: VarAttrs,
    /// [`Linkage::External`] or [`Linkage::Internal`].
    pub linkage: Linkage,
    pub init: VarInit,
    pub origin: Origin,
}

/// A macro's value: a constant, or a declaration it names.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MacroValue<'l> {
    /// The value's bits (as [`ConstRow::value`]) and its integer type: an integer type, `_Bool`,
    /// or a typedef or enum of one (`#define NPY_MAXDIMS ((int)64)` and a macro cast to an enum
    /// type are both constants of their type).
    Int { bits: u64, ty: TypeId },
    /// The value and its floating type (four or eight bytes), through typedefs.
    Float { value: f64, ty: TypeId },
    /// A narrow string's bytes, without the NUL C adds, and its character type.
    Str { bytes: &'l [u8], ty: TypeId },
    /// Entry `index` of the table `table` (a pointer or an array variable), cast to `ty` (a
    /// pointer type, through typedefs), read through when `deref`:
    /// `#define PyArray_Type (*(PyTypeObject *)PyArray_API[2])` (numpy __multiarray_api.h:652)
    /// and the function slot `(*(unsigned int (*)(void)) PyArray_API[0])` (:650).
    Slot { table: VarId, index: u64, ty: TypeId, deref: bool },
    /// The address of `variable`: `#define Py_None (&_Py_NoneStruct)` (cpython object.h:1106).
    AddressOf { variable: VarId },
}

/// Why a model is not one the builder takes: each names the row and what is wrong (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelError {
    /// More `what` than a `u32` numbers.
    TooLarge { what: &'static str },
    /// A type names a type, a declaration, a name or a header the builder has not made.
    Dangling { what: &'static str, index: u32 },
    /// A record or enum defined twice.
    Redefined { what: &'static str, index: u32 },
    /// Two declarations of one name in one namespace and table (C has one of each; an overload
    /// set's members differ by symbol).
    Duplicate { kind: DeclKind, name: Name },
    /// A bitfield whose unit is past its record's units, or whose bits leave the unit; a unit
    /// past its record's size.
    Bitfield { record: RecordId, field: u32 },
    /// A function declaration whose type is not a function type, or whose parameter names are
    /// not one per parameter.
    NotAFunction { name: Name },
    /// A type nested deeper than [`MOST_DEPTH`] (rule 18: a walk has a bound).
    TooDeep { ty: TypeId },
    /// A name that is not a C identifier (empty, or a byte outside `[A-Za-z0-9_$]`).
    BadName { name: Name },
    /// A header path [`crate::place::RelPath`] refuses.
    BadHeader { header: u32 },
    /// A linkage the declaration cannot have: an `inline` variable, or a variable of internal
    /// linkage that is only declared ([`VarInit::Absent`]: C makes a file-scope `static`
    /// a tentative definition, which is [`VarInit::Zero`]).
    BadLinkage { name: Name },
    /// A function defined twice (two rows of one symbol with a body), or two rows of one symbol
    /// whose types or linkages differ: not a declaration and its definition.
    Redeclared { name: Name },
    /// A [`MacroValue::Slot`] whose table is not a pointer or an array, or whose type is not a
    /// pointer.
    NotASlot { name: Name },
}

/// The deepest a type may nest (a pointer to an array of pointers to ..): far past any header's.
pub const MOST_DEPTH: u32 = 256;

/// What a [`DeclsBuilder`] is made from.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct DeclsOpts<'a> {
    /// The target facts the record carries ([`AbiRow`]).
    pub abi: AbiRow,
    pub alloc: Alloc<'a>,
}

/// The model a front end fills, every column in its allocator. Rows are in the order the front
/// end made them; [`crate::canon::encode`] writes them in the canonical order.
pub struct DeclsBuilder<'a> {
    pub(crate) abi: AbiRow,
    pub(crate) name_bytes: AVec<'a, u8>,
    pub(crate) name_ends: AVec<'a, u32>,
    pub(crate) headers: AVec<'a, HeaderRow>,
    pub(crate) namespaces: AVec<'a, NamespaceRow>,
    pub(crate) types: AVec<'a, TypeRow>,
    pub(crate) type_lists: AVec<'a, u32>,
    pub(crate) records: AVec<'a, RecordRow>,
    pub(crate) fields: AVec<'a, FieldRow>,
    pub(crate) units: AVec<'a, UnitRow>,
    pub(crate) bases: AVec<'a, BaseRow>,
    pub(crate) enums: AVec<'a, EnumRow>,
    pub(crate) consts: AVec<'a, ConstRow>,
    pub(crate) fns: AVec<'a, FnRow>,
    pub(crate) name_lists: AVec<'a, u32>,
    pub(crate) vars: AVec<'a, VarRow>,
    pub(crate) typedefs: AVec<'a, TypedefRow>,
    pub(crate) macros: AVec<'a, MacroRow>,
    pub(crate) strings: AVec<'a, u8>,
    pub(crate) unexpressed: AVec<'a, UnexpressedRow>,
    pub(crate) alloc: Alloc<'a>,
}

/// The domain of a header's path digest.
pub const PATH_DOMAIN: &str = "blaze-c header path";

fn count(len: usize, what: &'static str) -> Result<u32, ModelError> {
    narrow(len).ok_or(ModelError::TooLarge { what })
}

fn origin_parts(origin: Origin) -> (u32, u32, u32) {
    (origin.header.0, origin.at, origin.len)
}

impl<'a> DeclsBuilder<'a> {
    pub fn new(opts: DeclsOpts<'a>) -> DeclsBuilder<'a> {
        let alloc = opts.alloc;
        DeclsBuilder {
            abi: opts.abi,
            name_bytes: Vec::new_in(alloc),
            name_ends: Vec::new_in(alloc),
            headers: Vec::new_in(alloc),
            namespaces: Vec::new_in(alloc),
            types: Vec::new_in(alloc),
            type_lists: Vec::new_in(alloc),
            records: Vec::new_in(alloc),
            fields: Vec::new_in(alloc),
            units: Vec::new_in(alloc),
            bases: Vec::new_in(alloc),
            enums: Vec::new_in(alloc),
            consts: Vec::new_in(alloc),
            fns: Vec::new_in(alloc),
            name_lists: Vec::new_in(alloc),
            vars: Vec::new_in(alloc),
            typedefs: Vec::new_in(alloc),
            macros: Vec::new_in(alloc),
            strings: Vec::new_in(alloc),
            unexpressed: Vec::new_in(alloc),
            alloc,
        }
    }

    pub fn abi(&self) -> AbiRow {
        self.abi
    }

    /// The text of `name`.
    pub fn text(&self, name: Name) -> &str {
        let k = at(name.0);
        let start = match k {
            0 => 0,
            k => at(self.name_ends[k - 1]),
        };
        let bytes = &self.name_bytes[start..at(self.name_ends[k])];
        core::str::from_utf8(bytes).expect("the builder takes names as `&str`")
    }

    /// A name for `text`. Names are not shared here: [`crate::canon::encode`] numbers them once.
    pub fn name(&mut self, text: &str) -> Result<Name, ModelError> {
        let k = count(self.name_ends.len(), "names")?;
        let end = self
            .name_bytes
            .len()
            .checked_add(text.len())
            .and_then(narrow)
            .ok_or(ModelError::TooLarge { what: "name bytes" })?;
        if !is_c_identifier(text) {
            return Err(ModelError::BadName { name: Name(k) });
        }
        self.name_bytes.extend_from_slice(text.as_bytes());
        self.name_ends.push(end);
        Ok(Name(k))
    }

    /// The header at `path` under `root`, as the front end read it.
    pub fn header(&mut self, root: Root, path: &str) -> Result<HeaderId, ModelError> {
        let k = count(self.headers.len(), "headers")?;
        let rel = RelPath::new(path).map_err(|_| ModelError::BadHeader { header: k })?;
        self.headers.push(HeaderRow { path: crate::place::path_digest(root, rel), root: root.byte(), pad: [0; 3] });
        Ok(HeaderId(k))
    }

    /// A namespace `name` in `parent` (C++).
    pub fn namespace(&mut self, name: Name, parent: NsId) -> Result<NsId, ModelError> {
        self.name_ok(name)?;
        self.ns_ok(parent)?;
        let k = count(self.namespaces.len(), "namespaces")?;
        self.namespaces.push(NamespaceRow { name: name.0, parent: parent.0 });
        Ok(NsId(k))
    }

    /// The type `ty`. Equal types may be made twice; the record holds each once.
    pub fn ty(&mut self, ty: CType<'_>) -> Result<TypeId, ModelError> {
        let k = count(self.types.len(), "types")?;
        let row = |kind: u8, quals: Quals, a: u32, b: u32, c: u32, flags: u8, extra: u8| TypeRow {
            a,
            b,
            c,
            kind,
            quals: quals.0 & Quals::ALL,
            flags,
            extra,
        };
        let row = match ty {
            CType::Void { quals } => row(tag::VOID, quals, 0, 0, 0, 0, 0),
            CType::Bool { size, align, quals } => row(tag::BOOL, quals, size, align, 0, 0, 0),
            CType::Int { kind, size, align, signed, quals } => {
                row(tag::INT, quals, size, align, 0, if signed { tag::SIGNED } else { 0 }, kind.byte())
            }
            CType::Float { kind, size, align, quals } => row(tag::FLOAT, quals, size, align, 0, 0, kind.byte()),
            CType::Pointer { pointee, quals } => {
                self.ty_ok(pointee)?;
                row(tag::POINTER, quals, pointee.0, 0, 0, 0, 0)
            }
            CType::Reference { pointee, category } => {
                self.ty_ok(pointee)?;
                row(tag::REFERENCE, Quals::NONE, pointee.0, 0, 0, 0, category.byte())
            }
            CType::Array { elem, len, quals } => {
                self.ty_ok(elem)?;
                let (n, flags) = match len {
                    Some(n) => (n, 0),
                    None => (0, tag::INCOMPLETE),
                };
                let (lo, hi) = split(n);
                row(tag::ARRAY, quals, elem.0, lo, hi, flags, 0)
            }
            CType::Function { result, params, variadic, prototyped, conv } => {
                self.ty_ok(result)?;
                for param in params {
                    self.ty_ok(*param)?;
                }
                let start = count(self.type_lists.len(), "parameter lists")?;
                let len = count(params.len(), "parameters")?;
                count(self.type_lists.len() + params.len(), "parameter lists")?;
                for param in params {
                    self.type_lists.push(param.0);
                }
                let flags = if variadic { tag::VARIADIC } else { 0 } | if prototyped { tag::PROTOTYPED } else { 0 };
                row(tag::FUNCTION, Quals::NONE, result.0, start, len, flags, conv.byte())
            }
            CType::Record { record, quals } => {
                self.record_ok(record)?;
                row(tag::RECORD, quals, record.0, 0, 0, 0, 0)
            }
            CType::Enum { decl, quals } => {
                if at(decl.0) >= self.enums.len() {
                    return Err(ModelError::Dangling { what: "an enum", index: decl.0 });
                }
                row(tag::ENUM, quals, decl.0, 0, 0, 0, 0)
            }
            CType::Typedef { decl, quals } => {
                if at(decl.0) >= self.typedefs.len() {
                    return Err(ModelError::Dangling { what: "a typedef", index: decl.0 });
                }
                row(tag::TYPEDEF, quals, decl.0, 0, 0, 0, 0)
            }
            CType::VaList { lowered, quals } => {
                self.ty_ok(lowered)?;
                row(tag::VA_LIST, quals, lowered.0, 0, 0, 0, 0)
            }
            CType::Unexpressible { cause, size, align } => {
                row(tag::UNEXPRESSIBLE, Quals::NONE, size, align, 0, 0, cause.byte())
            }
        };
        self.types.push(row);
        Ok(TypeId(k))
    }

    /// A struct or union named `name` (`None`: anonymous), declared but not yet defined: an
    /// opaque handle until [`DeclsBuilder::define_record`].
    pub fn declare_record(
        &mut self,
        kind: RecordKind,
        name: Option<Name>,
        ns: NsId,
        origin: Origin,
    ) -> Result<RecordId, ModelError> {
        let name = self.opt_name(name)?;
        self.ns_ok(ns)?;
        self.header_ok(origin.header)?;
        let k = count(self.records.len(), "records")?;
        let (header, at, len) = origin_parts(origin);
        self.records.push(RecordRow {
            size: 0,
            name,
            ns: ns.0,
            fields_start: 0,
            fields_len: 0,
            units_start: 0,
            units_len: 0,
            bases_start: 0,
            bases_len: 0,
            align: 0,
            pack: 0,
            header,
            at,
            len,
            kind: kind.byte(),
            complete: 0,
            relocation: Relocation::TriviallyCopyable.byte(),
            pad: 0,
        });
        Ok(RecordId(k))
    }

    /// The definition of `record`, once.
    pub fn define_record(&mut self, record: RecordId, body: RecordBody<'_>) -> Result<(), ModelError> {
        self.record_ok(record)?;
        if self.records[at(record.0)].complete != 0 {
            return Err(ModelError::Redefined { what: "a record", index: record.0 });
        }
        for (k, field) in body.fields.iter().enumerate() {
            self.ty_ok(field.ty)?;
            if let Some(name) = field.name {
                self.name_ok(name)?;
            }
            if let Some(bits) = field.bitfield {
                let bad = ModelError::Bitfield { record, field: count(k, "fields")? };
                let Some(&(offset, size)) = body.units.get(at(bits.unit)) else { return Err(bad) };
                let start = u64::from(offset) * 8;
                let end = start + u64::from(size) * 8;
                let field_end = field.offset_bits.checked_add(u64::from(bits.width)).ok_or(bad)?;
                if field.offset_bits < start || field_end > end {
                    return Err(bad);
                }
            }
        }
        for (k, &(offset, size)) in body.units.iter().enumerate() {
            if u64::from(offset) + u64::from(size) > body.size {
                return Err(ModelError::Bitfield { record, field: count(k, "units")? });
            }
        }
        for (base, _, _) in body.bases {
            self.record_ok(*base)?;
        }
        let fields_start = count(self.fields.len(), "fields")?;
        let units_start = count(self.units.len(), "units")?;
        let bases_start = count(self.bases.len(), "bases")?;
        count(self.fields.len() + body.fields.len(), "fields")?;
        count(self.units.len() + body.units.len(), "units")?;
        count(self.bases.len() + body.bases.len(), "bases")?;
        for field in body.fields {
            let (width, unit, bitfield) = match field.bitfield {
                Some(bits) => (bits.width, units_start + bits.unit, 1),
                None => (0, NONE, 0),
            };
            self.fields.push(FieldRow {
                offset_bits: field.offset_bits,
                name: field.name.map_or(NONE, |n| n.0),
                ty: field.ty.0,
                width,
                unit,
                at: field.at,
                len: field.len,
                bitfield,
                pad: [0; 7],
            });
        }
        for &(offset, size) in body.units {
            self.units.push(UnitRow { offset, size });
        }
        for &(base, offset, virt) in body.bases {
            self.bases.push(BaseRow { offset, record: base.0, flags: u8::from(virt), pad: [0; 3] });
        }
        let row = &mut self.records[at(record.0)];
        row.size = body.size;
        row.align = body.align;
        row.pack = body.pack.unwrap_or(0);
        row.fields_start = fields_start;
        row.fields_len = count(body.fields.len(), "fields")?;
        row.units_start = units_start;
        row.units_len = count(body.units.len(), "units")?;
        row.bases_start = bases_start;
        row.bases_len = count(body.bases.len(), "bases")?;
        row.relocation = body.relocation.byte();
        row.complete = 1;
        Ok(())
    }

    /// An enum named `name` (`None`: anonymous), declared; [`DeclsBuilder::define_enum`] gives
    /// its constants.
    pub fn declare_enum(&mut self, name: Option<Name>, ns: NsId, origin: Origin) -> Result<EnumId, ModelError> {
        let name = self.opt_name(name)?;
        self.ns_ok(ns)?;
        self.header_ok(origin.header)?;
        let k = count(self.enums.len(), "enums")?;
        let (header, at, len) = origin_parts(origin);
        self.enums.push(EnumRow {
            name,
            ns: ns.0,
            underlying: NONE,
            consts_start: 0,
            consts_len: 0,
            header,
            at,
            len,
            complete: 0,
            scoped: 0,
            pad: [0; 2],
        });
        Ok(EnumId(k))
    }

    /// The underlying integer type and the constants of `decl`, once. For a forward-declared
    /// enum with a fixed underlying type (C23, C++), `consts` is empty and `complete` false.
    pub fn define_enum(
        &mut self,
        decl: EnumId,
        underlying: TypeId,
        consts: &[EnumConst],
        complete: bool,
        scoped: bool,
    ) -> Result<(), ModelError> {
        if at(decl.0) >= self.enums.len() {
            return Err(ModelError::Dangling { what: "an enum", index: decl.0 });
        }
        if self.enums[at(decl.0)].underlying != NONE {
            return Err(ModelError::Redefined { what: "an enum", index: decl.0 });
        }
        self.ty_ok(underlying)?;
        for c in consts {
            self.name_ok(c.name)?;
        }
        let start = count(self.consts.len(), "enum constants")?;
        count(self.consts.len() + consts.len(), "enum constants")?;
        for c in consts {
            self.consts.push(ConstRow { value: c.value, name: c.name.0, at: c.at, len: c.len, pad: [0; 4] });
        }
        let row = &mut self.enums[at(decl.0)];
        row.underlying = underlying.0;
        row.consts_start = start;
        row.consts_len = count(consts.len(), "enum constants")?;
        row.complete = u8::from(complete);
        row.scoped = u8::from(scoped);
        Ok(())
    }

    pub fn typedef(&mut self, name: Name, ns: NsId, ty: TypeId, origin: Origin) -> Result<TypedefId, ModelError> {
        self.name_ok(name)?;
        self.ns_ok(ns)?;
        self.ty_ok(ty)?;
        self.header_ok(origin.header)?;
        let k = count(self.typedefs.len(), "typedefs")?;
        let (header, at, len) = origin_parts(origin);
        self.typedefs.push(TypedefRow { name: name.0, ns: ns.0, ty: ty.0, header, at, len });
        Ok(TypedefId(k))
    }

    pub fn function(&mut self, f: Function<'_>) -> Result<(), ModelError> {
        self.name_ok(f.name)?;
        self.name_ok(f.symbol)?;
        self.ns_ok(f.ns)?;
        self.ty_ok(f.ty)?;
        self.header_ok(f.origin.header)?;
        let fn_ty = self.types[at(f.ty.0)];
        if fn_ty.kind != tag::FUNCTION && fn_ty.kind != tag::UNEXPRESSIBLE {
            return Err(ModelError::NotAFunction { name: f.name });
        }
        if fn_ty.kind == tag::FUNCTION && at(fn_ty.c) != f.param_names.len() {
            return Err(ModelError::NotAFunction { name: f.name });
        }
        for name in f.param_names.iter().flatten() {
            self.name_ok(*name)?;
        }
        let (owner, method) = match f.owner {
            Some((record, kind)) => {
                self.record_ok(record)?;
                (record.0, kind.byte())
            }
            None => (NONE, MethodKind::Free.byte()),
        };
        let start = count(self.name_lists.len(), "parameter names")?;
        count(self.name_lists.len() + f.param_names.len(), "parameter names")?;
        for name in f.param_names {
            self.name_lists.push(name.map_or(NONE, |n| n.0));
        }
        let (header, at, len) = origin_parts(f.origin);
        self.fns.push(FnRow {
            name: f.name.0,
            ns: f.ns.0,
            symbol: f.symbol.0,
            ty: f.ty.0,
            owner,
            params_start: start,
            params_len: count(f.param_names.len(), "parameter names")?,
            attrs: f.attrs.0 & FnAttrs::ALL,
            header,
            at,
            len,
            method,
            linkage: f.linkage.byte(),
            body: u8::from(f.body),
            pad: 0,
        });
        count(self.fns.len(), "functions")?;
        Ok(())
    }

    pub fn variable(&mut self, v: Variable) -> Result<VarId, ModelError> {
        self.name_ok(v.name)?;
        self.name_ok(v.symbol)?;
        self.ns_ok(v.ns)?;
        self.ty_ok(v.ty)?;
        self.header_ok(v.origin.header)?;
        match (v.linkage, v.init) {
            (Linkage::Inline, _) | (Linkage::Internal, VarInit::Absent) => {
                return Err(ModelError::BadLinkage { name: v.name });
            }
            _ => {}
        }
        let (init, init_bits) = match v.init {
            VarInit::Absent => (init::ABSENT, 0),
            VarInit::Zero => (init::ZERO, 0),
            VarInit::Int(bits) => (init::INT, bits),
            VarInit::Other => (init::OTHER, 0),
        };
        let k = count(self.vars.len(), "variables")?;
        let (header, at, len) = origin_parts(v.origin);
        self.vars.push(VarRow {
            init_bits,
            name: v.name.0,
            ns: v.ns.0,
            symbol: v.symbol.0,
            ty: v.ty.0,
            attrs: v.attrs.0 & VarAttrs::ALL,
            header,
            at,
            len,
            linkage: v.linkage.byte(),
            init,
            pad: [0; 6],
        });
        Ok(VarId(k))
    }

    /// An object-like macro that is a constant, or that names a variable (a table's slot, a
    /// variable's address).
    pub fn constant(&mut self, name: Name, value: MacroValue<'_>, origin: Origin) -> Result<(), ModelError> {
        self.name_ok(name)?;
        self.header_ok(origin.header)?;
        let (kind, ty, bits, var, deref, str_start, str_len) = match value {
            MacroValue::Int { bits, ty } => (value::INT, ty, bits, NONE, 0, 0, 0),
            MacroValue::Float { value: v, ty } => {
                self.ty_ok(ty)?;
                let bits = match self.types[at(self.resolve(ty)?.0)].a {
                    4 => u64::from(to_f32(v).to_bits()),
                    _ => v.to_bits(),
                };
                (value::FLOAT, ty, bits, NONE, 0, 0, 0)
            }
            MacroValue::Str { bytes, ty } => {
                let start = count(self.strings.len(), "string bytes")?;
                count(self.strings.len() + bytes.len(), "string bytes")?;
                self.strings.extend_from_slice(bytes);
                (value::STR, ty, 0, NONE, 0, start, count(bytes.len(), "string bytes")?)
            }
            MacroValue::Slot { table, index, ty, deref } => {
                self.var_ok(table)?;
                self.ty_ok(ty)?;
                let table_ty = self.types[at(self.resolve(TypeId(self.vars[at(table.0)].ty))?.0)];
                let cast = self.types[at(self.resolve(ty)?.0)];
                if !matches!(table_ty.kind, tag::POINTER | tag::ARRAY) || cast.kind != tag::POINTER {
                    return Err(ModelError::NotASlot { name });
                }
                (value::SLOT, ty, index, table.0, u8::from(deref), 0, 0)
            }
            MacroValue::AddressOf { variable } => {
                self.var_ok(variable)?;
                let ty = TypeId(self.vars[at(variable.0)].ty);
                (value::ADDRESS_OF, ty, 0, variable.0, 0, 0, 0)
            }
        };
        self.ty_ok(ty)?;
        let (header, at, len) = origin_parts(origin);
        self.macros.push(MacroRow {
            value: bits,
            name: name.0,
            ty: ty.0,
            var,
            str_start,
            str_len,
            header,
            at,
            len,
            kind,
            deref,
            pad: [0; 6],
        });
        count(self.macros.len(), "macros")?;
        Ok(())
    }

    /// `ty` with typedefs looked through (rule 18: at most [`MOST_DEPTH`] of them).
    fn resolve(&self, mut ty: TypeId) -> Result<TypeId, ModelError> {
        for _ in 0..MOST_DEPTH {
            let row = self.types[at(ty.0)];
            match row.kind {
                tag::TYPEDEF => ty = TypeId(self.typedefs[at(row.a)].ty),
                _ => return Ok(ty),
            }
        }
        Err(ModelError::TooDeep { ty })
    }

    fn var_ok(&self, var: VarId) -> Result<(), ModelError> {
        match at(var.0) < self.vars.len() {
            true => Ok(()),
            false => Err(ModelError::Dangling { what: "a variable", index: var.0 }),
        }
    }

    /// A declaration the model cannot hold, by its cause.
    pub fn unexpressed(&mut self, name: Name, ns: NsId, kind: DeclKind, cause: Unexpressed, origin: Origin) -> Result<(), ModelError> {
        self.name_ok(name)?;
        self.ns_ok(ns)?;
        self.header_ok(origin.header)?;
        let (header, at, len) = origin_parts(origin);
        self.unexpressed.push(UnexpressedRow {
            name: name.0,
            ns: ns.0,
            header,
            at,
            len,
            kind: kind.byte(),
            cause: cause.byte(),
            pad: [0; 2],
        });
        count(self.unexpressed.len(), "unexpressed declarations")?;
        Ok(())
    }

    fn opt_name(&self, name: Option<Name>) -> Result<u32, ModelError> {
        match name {
            Some(name) => self.name_ok(name).map(|()| name.0),
            None => Ok(NONE),
        }
    }

    fn name_ok(&self, name: Name) -> Result<(), ModelError> {
        match at(name.0) < self.name_ends.len() {
            true => Ok(()),
            false => Err(ModelError::Dangling { what: "a name", index: name.0 }),
        }
    }

    fn ty_ok(&self, ty: TypeId) -> Result<(), ModelError> {
        match at(ty.0) < self.types.len() {
            true => Ok(()),
            false => Err(ModelError::Dangling { what: "a type", index: ty.0 }),
        }
    }

    fn record_ok(&self, record: RecordId) -> Result<(), ModelError> {
        match at(record.0) < self.records.len() {
            true => Ok(()),
            false => Err(ModelError::Dangling { what: "a record", index: record.0 }),
        }
    }

    fn ns_ok(&self, ns: NsId) -> Result<(), ModelError> {
        match ns == NsId::GLOBAL || at(ns.0) < self.namespaces.len() {
            true => Ok(()),
            false => Err(ModelError::Dangling { what: "a namespace", index: ns.0 }),
        }
    }

    fn header_ok(&self, header: HeaderId) -> Result<(), ModelError> {
        match at(header.0) < self.headers.len() {
            true => Ok(()),
            false => Err(ModelError::Dangling { what: "a header", index: header.0 }),
        }
    }
}

/// `n`'s low and high words.
pub(crate) fn split(n: u64) -> (u32, u32) {
    let lo = u32::try_from(n & u64::from(u32::MAX)).expect("masked to 32 bits");
    let hi = u32::try_from(n >> 32).expect("shifted to 32 bits");
    (lo, hi)
}

/// The `u64` of a low and a high word.
pub(crate) fn join(lo: u32, hi: u32) -> u64 {
    u64::from(lo) | (u64::from(hi) << 32)
}

/// `v` as the nearest `f32`: what a C front end gives for a `float`-typed macro.
#[expect(blaze::as_cast, reason = "`as` is the one rounding conversion from f64 to f32 in core")]
fn to_f32(v: f64) -> f32 {
    v as f32
}

/// Whether `text` is an identifier C (and the `$` extension) allows: what every name of the
/// model is, so the rendering never has to escape one.
pub fn is_c_identifier(text: &str) -> bool {
    let bytes = text.as_bytes();
    match bytes.first() {
        None => false,
        Some(b) if b.is_ascii_digit() => false,
        Some(_) => bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$'),
    }
}

/// The digest of a header's content: what the read list holds for a header that was there.
pub fn content_digest(bytes: &[u8]) -> ContentId {
    let mut hasher = Hasher::new("blaze-c header content");
    hasher.update(bytes);
    hasher.finish()
}
