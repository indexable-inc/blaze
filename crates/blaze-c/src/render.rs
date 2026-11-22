//! A declaration record to Rust text: the form the expansion pipeline takes (it lexes the text,
//! `blaze_expand::tokens::lex`), deterministic (the record's order, nothing else), with every
//! declaration it leaves out, or renders only in part, as a [`Finding`].
//!
//! The choices, and why:
//! - `pub mod NAME { .. }` with the lints of C's naming allowed inside it: C names are kept as
//!   written, so a reader searches the header and the Rust for one name.
//! - Functions and data in `unsafe extern "ABI" { .. }` blocks, one a calling convention, each
//!   item `pub unsafe fn` / `pub unsafe static`: a call into C is unsafe, and the block says so.
//!   `#[link_name]` where the symbol is not the name; `"\u{1}"` before a literal symbol (an asm
//!   label), which no platform prefix is added to. `#[link(name = ..)]` from `link`.
//! - Every C scalar by its `::core::ffi` name (`c_long`, not `i64`), and every Rust primitive by
//!   `::core::primitive` (a header's `typedef int bool;` would otherwise shadow `bool`); a
//!   static assert per scalar kind that `core`'s size is the record's, so a record for one target
//!   cannot compile silently for another.
//! - A struct or union as `#[repr(C)]` (with `packed(N)` or `align(N)` where the front end's
//!   layout needs them), its fields at the front end's offsets (explicit `__pad_N` bytes where
//!   `repr(C)` alone would place a field earlier), and a static assert of its size, its alignment
//!   and each field's offset. A layout `repr(C)` cannot reproduce is the record's bytes,
//!   `__bytes: [u8; SIZE]` at its alignment, and a finding.
//! - A bitfield as its storage unit (`__bitfield_N`, the unsigned integer of the unit's size)
//!   and two accessors (`x()`, `set_x()`), with the target's bit order.
//! - An enum as `#[repr(transparent)] pub struct E(pub UNDERLYING)` with an associated const a
//!   constant: C's enums are integers (any value is a valid `E`), which a Rust `enum` is not.
//!   An anonymous enum's constants are module constants of its underlying type.
//! - An opaque (declared, never defined) struct as a zero-sized struct with a private field
//!   and a `PhantomData<(*mut u8, PhantomPinned)>`: it cannot be built, moved out of a pointer,
//!   sent or shared, only pointed at.
//! - A pointer to a function as `Option<unsafe extern "ABI" fn(..) -> R>`: C's function
//!   pointers are nullable, and `Option` of a fn pointer is a pointer.
//! - A keyword as a raw identifier (`r#type`); a name not even a raw identifier can spell is a
//!   finding ([`Unexpressed::ReservedName`]): a declaration is dropped, a field renamed
//!   `__field_N` and a parameter written `_`.
//! - C++ parts of the record (namespaces, overloads, methods, bases, a relocation fact other
//!   than trivially copyable, references, may-throw) are not rendered yet: each a finding of
//!   its own cause (`Cpp*`), never a silent skip.
//! - What the import's own unit holds ([`UnitKey`]) in `unsafe extern` blocks of their own, with
//!   no `#[link]` (the unit is the import's, not the library's), each item bound to the unit's
//!   symbol for it by `#[link_name]`: a function the headers define with internal or inline
//!   linkage as a normal `pub unsafe fn` (its thunk); a header-local variable as
//!   `pub safe fn NAME() -> *mut T`, its address (no `static mut`: the variable's one
//!   instance is the unit's, and a raw pointer is the one honest handle on data that C code
//!   writes behind Rust's back); a `macro_fn` as a `pub unsafe fn` (its wrapper).
//! - A macro naming a variable's address (`#define Py_None (&_Py_NoneStruct)`) as a safe
//!   `pub fn NAME() -> *mut T`: a raw pointer to a static is safe to make, and a function, not
//!   a `const`, because a static's address is not a constant on every target (an extern
//!   static's is a relocation, and Rust refuses a `const` that reads one).
//! - A macro naming a table's slot (`#define PyArray_Type (*(PyTypeObject *)PyArray_API[2])`)
//!   as a `pub unsafe fn` that loads the table, checks it is not null (naming the table) and
//!   returns the entry as the macro's pointer type; a function slot as a `pub unsafe fn` of the
//!   function's own parameters that calls the entry, so C's `PyArray_GetNDArrayCVersion()` is
//!   Rust's too. Unsafe, never safe: the table is null until the library's import call has
//!   filled it (numpy's `import_array()`), and only the header vouches for the entry's type.
//!   Its doc says so under `# Safety`.

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{AString, AVec, Alloc, String, Vec, format_into};

use crate::finding::Finding;
use crate::grammar::{MacroType, Request};
use crate::key::UnitKey;
use crate::model::{
    Conv, DeclKind, FloatKind, FnAttrs, FnRow, IntKind, Linkage, MOST_DEPTH, MacroRow, MethodKind, Quals,
    RecordKind, Relocation, TypeRow, Unexpressed, VarAttrs, VarRow, join, tag, value,
};
use crate::view::{DeclsView, ViewError};
use crate::{NONE, at, narrow};

/// Why a record cannot be rendered at all (a declaration that cannot is a finding instead).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RenderError {
    View(ViewError),
    /// More declarations than a row index holds.
    TooLarge,
    /// A declaration that rendered when its dependencies settled failed on the final pass: a
    /// bug of this module, named by its table and row.
    Unsettled { kind: DeclKind, row: u32, cause: Unexpressed },
    /// A `macro_fn` of the request that cannot be what it asks for, by its index in the
    /// request's (sorted) `macro_fns`: the program asked for it by name, so it is refused, not
    /// left out as a finding.
    MacroFn { index: u32, fault: MacroFnFault },
}

/// Why a `macro_fn` is refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MacroFnFault {
    /// The headers have no function-like macro of the name.
    NoSuchMacro,
    /// The macro is one no function has the meaning of ([`Unexpressed::MacroIsFragment`],
    /// [`Unexpressed::MacroPastesOrAssigns`]), by the front end's cause.
    NotAFunction(Unexpressed),
    /// The headers declare a function of the name: it is the macro's meaning, and it renders.
    IsAFunction,
    /// The named type is not a `core::ffi` scalar or a type the module renders (by value, one
    /// whose layout is exact): `part` is the parameter's index, [`NONE`] for the result.
    UnknownType { part: u32 },
    /// The rendering has no unit to ask for the wrapper in (see [`UnitKey`]).
    NoUnit,
    /// Another rendered declaration has the name.
    NameClash,
}

/// A rendered module and its findings.
pub struct Rendered<'a> {
    pub rust: AString<'a>,
    pub findings: AVec<'a, Finding>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Keep,
    Drop(Unexpressed),
}

/// A rendering failure inside a declaration: the cause, and the part (field or parameter)
/// it is at, [`NONE`] for the whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fail {
    cause: Unexpressed,
    part: u32,
}

fn fail(cause: Unexpressed) -> Fail {
    Fail { cause, part: NONE }
}

const STRICT: &[&str] = &[
    "as", "break", "const", "continue", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let",
    "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "static", "struct", "trait", "true", "type",
    "unsafe", "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];

const UNSPELLABLE: &[&str] = &["self", "Self", "super", "crate", "_"];

/// `name` as a Rust identifier into `out`: raw for a keyword, refused for a name no identifier
/// spells.
fn ident(out: &mut AString<'_>, name: &str) -> Result<(), Fail> {
    if UNSPELLABLE.contains(&name) {
        return Err(fail(Unexpressed::ReservedName));
    }
    if STRICT.contains(&name) {
        out.push_str("r#");
    }
    out.push_str(name);
    Ok(())
}

fn round_up(n: u64, align: u64) -> u64 {
    match align {
        0 | 1 => n,
        a => n.div_ceil(a) * a,
    }
}

/// The unsigned primitive of a storage unit's size.
fn unsigned_of(size: u32) -> Option<(&'static str, &'static str, u32)> {
    match size {
        1 => Some(("u8", "i8", 8)),
        2 => Some(("u16", "i16", 16)),
        4 => Some(("u32", "i32", 32)),
        8 => Some(("u64", "i64", 64)),
        16 => Some(("u128", "i128", 128)),
        _ => None,
    }
}

fn c_int_name(kind: IntKind) -> &'static str {
    match kind {
        IntKind::Char => "::core::ffi::c_char",
        IntKind::SChar => "::core::ffi::c_schar",
        IntKind::UChar => "::core::ffi::c_uchar",
        IntKind::Short => "::core::ffi::c_short",
        IntKind::UShort => "::core::ffi::c_ushort",
        IntKind::Int => "::core::ffi::c_int",
        IntKind::UInt => "::core::ffi::c_uint",
        IntKind::Long => "::core::ffi::c_long",
        IntKind::ULong => "::core::ffi::c_ulong",
        IntKind::LongLong => "::core::ffi::c_longlong",
        IntKind::ULongLong => "::core::ffi::c_ulonglong",
        IntKind::Int128 => "::core::primitive::i128",
        IntKind::UInt128 => "::core::primitive::u128",
    }
}

fn abi_name(conv: Conv) -> Result<&'static str, Fail> {
    match conv {
        Conv::C => Ok("C"),
        Conv::Win64 => Ok("win64"),
        Conv::SysV64 => Ok("sysv64"),
        Conv::Stdcall => Ok("stdcall"),
        Conv::Fastcall => Ok("fastcall"),
        Conv::Thiscall => Ok("thiscall"),
        Conv::Aapcs => Ok("aapcs"),
        Conv::Vectorcall => Err(fail(Unexpressed::UnstableConvention)),
    }
}

/// How a record is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Form {
    /// Declared, never defined.
    Opaque,
    /// Its fields, as the front end laid them out; `exact` when no byte was added that the C
    /// type does not have (so passing it by value is classified as C classifies it).
    Fields { exact: bool },
    /// Its bytes alone.
    Bytes,
}

/// What is known of a type's layout: size and alignment in bytes.
type Layout = Option<(u64, u64)>;

struct R<'v, 'r, 'a> {
    v: DeclsView<'v>,
    request: &'r Request<'r>,
    rec: AVec<'a, Status>,
    form: AVec<'a, Form>,
    /// A record's rendered name: its own, the typedef's that names it, else `__anon_N`.
    rec_name: AVec<'a, u32>,
    enums: AVec<'a, Status>,
    /// An enum's rendered name (its own or its typedef's); [`NONE`]: rendered as its integer.
    enum_name: AVec<'a, u32>,
    typedefs: AVec<'a, Status>,
    /// A typedef that only repeats its record's or enum's name (`typedef struct s s;`).
    alias: AVec<'a, bool>,
    fns: AVec<'a, Status>,
    vars: AVec<'a, Status>,
    macros: AVec<'a, Status>,
    findings: AVec<'a, Finding>,
    /// The import's unit, which local functions, header-local variables and `macro_fn`s are
    /// bound to; `None`: there is none, and each is a finding (a `macro_fn` an error).
    unit: Option<UnitKey>,
    alloc: Alloc<'a>,
}

impl<'v, 'r, 'a> R<'v, 'r, 'a> {
    fn ty(&self, k: u32) -> TypeRow {
        self.v.types[at(k)]
    }

    /// `k` with typedefs looked through.
    fn resolve(&self, mut k: u32) -> u32 {
        for _ in 0..MOST_DEPTH {
            let row = self.ty(k);
            match row.kind {
                tag::TYPEDEF => k = self.v.typedefs[at(row.a)].ty,
                _ => return k,
            }
        }
        k
    }

    /// Whether `k` is const-qualified, through typedefs.
    fn is_const(&self, mut k: u32) -> bool {
        for _ in 0..MOST_DEPTH {
            let row = self.ty(k);
            if row.quals & Quals::CONST.0 != 0 {
                return true;
            }
            match row.kind {
                tag::TYPEDEF => k = self.v.typedefs[at(row.a)].ty,
                _ => return false,
            }
        }
        false
    }

    fn layout(&self, k: u32, depth: u32) -> Layout {
        if depth > MOST_DEPTH {
            return None;
        }
        let row = self.ty(k);
        let abi = self.v.abi;
        match row.kind {
            tag::BOOL | tag::INT | tag::FLOAT => Some((u64::from(row.a), u64::from(row.b))),
            tag::POINTER | tag::REFERENCE => Some((u64::from(abi.pointer_size), u64::from(abi.pointer_align))),
            tag::ARRAY => {
                let (size, align) = self.layout(row.a, depth + 1)?;
                match row.flags & tag::INCOMPLETE {
                    0 => Some((size.checked_mul(join(row.b, row.c))?, align)),
                    _ => Some((0, align)),
                }
            }
            tag::RECORD => {
                let r = self.v.records[at(row.a)];
                (r.complete == 1).then_some((r.size, u64::from(r.align)))
            }
            tag::ENUM => match self.v.enums[at(row.a)].underlying {
                NONE => None,
                u => self.layout(u, depth + 1),
            },
            tag::TYPEDEF => self.layout(self.v.typedefs[at(row.a)].ty, depth + 1),
            tag::VA_LIST => self.layout(row.a, depth + 1),
            tag::UNEXPRESSIBLE => (row.a > 0 && row.b > 0).then_some((u64::from(row.a), u64::from(row.b))),
            _ => None,
        }
    }

    fn rec_ident(&self, out: &mut AString<'_>, k: u32) -> Result<(), Fail> {
        match self.rec_name[at(k)] {
            NONE => {
                let first_anon = self.v.records.iter().take_while(|r| r.name != NONE).count();
                format_into!(out, "__anon_{}", at(k) - first_anon);
                Ok(())
            }
            name => ident(out, self.v.name(name)),
        }
    }

    /// Whether the module renders a type named `name` (a record, an enum or a typedef), usable
    /// as a parameter or result by value when `by_value` (a record then of exact layout).
    fn renders_type(&self, name: &str, by_value: bool) -> bool {
        let named = |n: u32| n != NONE && self.v.name(n) == name;
        let record = (0..self.v.records.len()).any(|k| {
            named(self.rec_name[k])
                && self.rec[k] == Status::Keep
                && (!by_value || self.form[k] == Form::Fields { exact: true })
        });
        let enumeration = (0..self.v.enums.len()).any(|k| named(self.enum_name[k]) && self.enums[k] == Status::Keep);
        let typedef = self
            .v
            .typedefs
            .iter()
            .enumerate()
            .any(|(k, t)| named(t.name) && self.typedefs[k] == Status::Keep && !self.alias[k]);
        record || enumeration || typedef
    }

    /// The text of type `k` into `out`. `by_value`: the type is a value here (a field, a
    /// parameter, a result, data), not a pointee; `call`: a parameter or result of a function,
    /// where a record's ABI must be exact.
    fn type_text(&self, out: &mut AString<'_>, k: u32, by_value: bool, call: bool, depth: u32) -> Result<(), Fail> {
        if depth > MOST_DEPTH {
            return Err(fail(Unexpressed::NotAValue));
        }
        let row = self.ty(k);
        match row.kind {
            tag::VOID => match by_value {
                true => Err(fail(Unexpressed::NotAValue)),
                false => {
                    out.push_str("::core::ffi::c_void");
                    Ok(())
                }
            },
            tag::BOOL => {
                out.push_str("::core::primitive::bool");
                Ok(())
            }
            tag::INT => {
                let kind = IntKind::from_byte(row.extra).expect("checked by open");
                out.push_str(c_int_name(kind));
                Ok(())
            }
            tag::FLOAT => {
                let kind = FloatKind::from_byte(row.extra).expect("checked by open");
                let name = match (kind, row.a) {
                    (FloatKind::Float, 4) => "::core::primitive::f32",
                    (FloatKind::Double | FloatKind::LongDouble, 8) => "::core::primitive::f64",
                    (FloatKind::Float16, _) => return Err(fail(Unexpressed::Float16)),
                    (FloatKind::Float128, _) => return Err(fail(Unexpressed::Float128)),
                    _ => return Err(fail(Unexpressed::LongDouble)),
                };
                out.push_str(name);
                Ok(())
            }
            tag::POINTER => {
                let target = self.resolve(row.a);
                if self.ty(target).kind == tag::FUNCTION {
                    out.push_str("::core::option::Option<");
                    self.fn_pointer(out, target, depth + 1)?;
                    out.push_str(">");
                    return Ok(());
                }
                out.push_str(if self.is_const(row.a) { "*const " } else { "*mut " });
                self.type_text(out, row.a, false, false, depth + 1)
            }
            tag::REFERENCE => Err(fail(Unexpressed::CppReference)),
            tag::ARRAY => {
                out.push_str("[");
                self.type_text(out, row.a, true, false, depth + 1)?;
                match row.flags & tag::INCOMPLETE {
                    0 => format_into!(out, "; {}]", join(row.b, row.c)),
                    _ => out.push_str("; 0]"),
                }
                Ok(())
            }
            tag::FUNCTION => Err(fail(Unexpressed::FunctionTypedef)),
            tag::RECORD => {
                if self.rec[at(row.a)] != Status::Keep {
                    return Err(fail(Unexpressed::DependsOnDropped));
                }
                match (self.form[at(row.a)], by_value, call) {
                    (Form::Opaque, true, _) => return Err(fail(Unexpressed::IncompleteByValue)),
                    (Form::Bytes | Form::Fields { exact: false }, true, true) => {
                        return Err(fail(Unexpressed::ByValueNotExact));
                    }
                    _ => {}
                }
                self.rec_ident(out, row.a)
            }
            tag::ENUM => {
                if self.enums[at(row.a)] != Status::Keep {
                    return Err(fail(Unexpressed::DependsOnDropped));
                }
                let e = self.v.enums[at(row.a)];
                match (self.enum_name[at(row.a)], e.underlying) {
                    (NONE, NONE) => Err(fail(Unexpressed::IncompleteByValue)),
                    (NONE, u) => self.type_text(out, u, by_value, call, depth + 1),
                    (name, _) => ident(out, self.v.name(name)),
                }
            }
            tag::TYPEDEF => {
                if self.typedefs[at(row.a)] != Status::Keep && !self.alias[at(row.a)] {
                    return Err(fail(Unexpressed::DependsOnDropped));
                }
                // The checks a typedef's target needs here, by value or in a call.
                let target = self.v.typedefs[at(row.a)].ty;
                let mut scratch = String::new_in(self.alloc);
                self.type_text(&mut scratch, target, by_value, call, depth + 1)?;
                match self.alias[at(row.a)] {
                    true => out.push_str(scratch.as_str()),
                    false => ident(out, self.v.name(self.v.typedefs[at(row.a)].name))?,
                }
                Ok(())
            }
            tag::VA_LIST => self.type_text(out, row.a, by_value, call, depth + 1),
            tag::UNEXPRESSIBLE => Err(fail(Unexpressed::from_byte(row.extra).expect("checked by open"))),
            _ => Err(fail(Unexpressed::NotAValue)),
        }
    }

    /// `unsafe extern "ABI" fn(..) -> R` for function type `k`.
    fn fn_pointer(&self, out: &mut AString<'_>, k: u32, depth: u32) -> Result<(), Fail> {
        let row = self.ty(k);
        if row.flags & tag::PROTOTYPED == 0 {
            return Err(fail(Unexpressed::Unprototyped));
        }
        let conv = Conv::from_byte(row.extra).expect("checked by open");
        let variadic = row.flags & tag::VARIADIC != 0;
        if variadic && conv != Conv::C {
            return Err(fail(Unexpressed::VariadicConvention));
        }
        format_into!(out, "unsafe extern \"{}\" fn(", abi_name(conv)?);
        for (j, param) in self.v.params_of(&row).iter().enumerate() {
            if j > 0 {
                out.push_str(", ");
            }
            self.type_text(out, *param, true, true, depth + 1)
                .map_err(|f| Fail { cause: f.cause, part: narrow(j).unwrap_or(NONE) })?;
        }
        if variadic {
            out.push_str(if row.c == 0 { "..." } else { ", ..." });
        }
        out.push_str(")");
        self.result(out, row.a, false, depth)
    }

    /// ` -> R` (nothing for `void`, `-> !` for a function that does not return).
    fn result(&self, out: &mut AString<'_>, k: u32, noreturn: bool, depth: u32) -> Result<(), Fail> {
        if noreturn {
            out.push_str(" -> !");
            return Ok(());
        }
        if self.ty(self.resolve(k)).kind == tag::VOID {
            return Ok(());
        }
        out.push_str(" -> ");
        self.type_text(out, k, true, true, depth + 1)
    }

    fn header_finding(&mut self, kind: DeclKind, name: u32, ns: u32, header: u32, start: u32, len: u32, f: Fail) {
        self.findings.push(Finding { kind, name, ns, cause: f.cause, header, at: start, len, part: f.part });
    }

    // ------------------------------------------------------------------- records

    /// The fields of record `k` into `out`, its asserts into `asserts`, its accessors into
    /// `accessors`; the findings of its parts into `parts`. The record's form is decided here.
    fn record_body(
        &self,
        k: u32,
        out: &mut AString<'_>,
        asserts: &mut AString<'_>,
        accessors: &mut AString<'_>,
        parts: &mut AVec<'_, Fail>,
    ) -> Result<Form, Fail> {
        let r = self.v.records[at(k)];
        if r.complete == 0 {
            return Ok(Form::Opaque);
        }
        let union = r.kind == RecordKind::Union.byte();
        let little = self.v.abi.little_endian == 1;
        let pack = u64::from(r.pack);
        let mut exact = true;
        let mut cur = 0u64;
        let mut max_align = 1u64;
        let mut name = String::new_in(self.alloc);
        self.rec_ident(&mut name, k)?;
        let fields = self.v.fields_of(&r);
        let units = self.v.units_of(&r);
        // Slots in offset order: each plain field, and each unit (once, at its first bitfield).
        let mut pad = 0usize;
        let mut unit_done: AVec<'_, bool> = Vec::new_in(self.alloc);
        unit_done.resize(units.len(), false);
        let mut ok = true;
        for (j, field) in fields.iter().enumerate() {
            let part = narrow(j).unwrap_or(NONE);
            let (want, size, align, text) = match field.bitfield {
                1 => {
                    let u = at(field.unit - r.units_start);
                    if unit_done[u] {
                        continue;
                    }
                    unit_done[u] = true;
                    let unit = units[u];
                    let mut text = String::new_in(self.alloc);
                    let align = match unsigned_of(unit.size) {
                        Some((prim, _, _)) if unit.offset % unit.size == 0 => {
                            format_into!(&mut text, "::core::primitive::{prim}");
                            u64::from(unit.size)
                        }
                        _ => {
                            format_into!(&mut text, "[::core::primitive::u8; {}]", unit.size);
                            exact = false;
                            1
                        }
                    };
                    let mut field_name = String::new_in(self.alloc);
                    format_into!(&mut field_name, "__bitfield_{u}");
                    (u64::from(unit.offset), u64::from(unit.size), align, (field_name, text))
                }
                _ => {
                    let mut field_name = String::new_in(self.alloc);
                    match field.name {
                        NONE => format_into!(&mut field_name, "__anon_{j}"),
                        n => {
                            if ident(&mut field_name, self.v.name(n)).is_err() {
                                field_name.clear();
                                format_into!(&mut field_name, "__field_{j}");
                                parts.push(Fail { cause: Unexpressed::ReservedName, part });
                            }
                        }
                    }
                    let layout = self.layout(field.ty, 0);
                    let mut text = String::new_in(self.alloc);
                    let (size, align) = match (self.type_text(&mut text, field.ty, true, false, 0), layout) {
                        (Ok(()), Some((size, align))) => (size, align),
                        (Err(f), Some((size, _))) => {
                            text.clear();
                            format_into!(&mut text, "[::core::primitive::u8; {size}]");
                            parts.push(Fail { cause: f.cause, part });
                            exact = false;
                            (size, 1)
                        }
                        (_, None) => {
                            parts.push(Fail { cause: Unexpressed::LayoutNotReproduced, part });
                            ok = false;
                            break;
                        }
                    };
                    let offset_bits = field.offset_bits;
                    if offset_bits % 8 != 0 {
                        ok = false;
                        break;
                    }
                    (offset_bits / 8, size, align, (field_name, text))
                }
            };
            let eff = if pack > 0 { align.min(pack) } else { align };
            max_align = max_align.max(eff);
            if union {
                if want != 0 {
                    ok = false;
                    break;
                }
                cur = cur.max(size);
            } else {
                let natural = round_up(cur, eff);
                if natural > want || want % eff != 0 {
                    ok = false;
                    break;
                }
                if natural < want {
                    format_into!(out, "    __pad_{pad}: [::core::primitive::u8; {}],\n", want - cur);
                    pad += 1;
                    exact = false;
                }
                cur = want + size;
            }
            format_into!(out, "    pub {}: {},\n", text.0.as_str(), text.1.as_str());
            // A union's members are all at zero: nothing to assert.
            if field.bitfield == 0 && !union {
                format_into!(
                    asserts,
                    "    ::core::assert!(::core::mem::offset_of!({}, {}) == {});\n",
                    name.as_str(),
                    text.0.as_str(),
                    want
                );
            }
        }
        let align = u64::from(r.align);
        if ok {
            let natural_align = if pack > 0 { max_align.min(pack) } else { max_align };
            // More alignment than the fields give is `align(N)`, which `packed` cannot carry.
            if (align > natural_align && pack > 0) || align < natural_align {
                ok = false;
            }
            let natural_size = round_up(cur, align);
            if ok && natural_size != r.size {
                if r.size > cur && r.size % align.max(1) == 0 {
                    // A union's padding is one more member, the whole size.
                    let len = if union { r.size } else { r.size - cur };
                    format_into!(out, "    __pad_{pad}: [::core::primitive::u8; {len}],\n");
                    exact = false;
                } else {
                    ok = false;
                }
            }
        }
        if !ok {
            out.clear();
            asserts.clear();
            accessors.clear();
            parts.push(Fail { cause: Unexpressed::LayoutNotReproduced, part: NONE });
            format_into!(out, "    __bytes: [::core::primitive::u8; {}],\n", r.size);
            return Ok(Form::Bytes);
        }
        // Bitfield accessors.
        for (j, field) in fields.iter().enumerate() {
            if field.bitfield == 0 || field.name == NONE || field.width == 0 {
                continue;
            }
            let part = narrow(j).unwrap_or(NONE);
            let u = at(field.unit - r.units_start);
            let unit = units[u];
            let Some((prim, signed_prim, bits)) = unsigned_of(unit.size).filter(|_| unit.offset % unit.size == 0) else {
                parts.push(Fail { cause: Unexpressed::BitfieldWindow, part });
                continue;
            };
            let width = u64::from(field.width);
            let start = u64::from(unit.offset) * 8;
            let Some(within) = field.offset_bits.checked_sub(start).filter(|w| w + width <= u64::from(bits)) else {
                parts.push(Fail { cause: Unexpressed::BitfieldWindow, part });
                continue;
            };
            let shift = match little {
                true => within,
                false => u64::from(bits) - within - width,
            };
            let mut ty = String::new_in(self.alloc);
            if self.type_text(&mut ty, field.ty, true, false, 0).is_err() {
                parts.push(Fail { cause: Unexpressed::Bitfield, part });
                continue;
            }
            let target = self.ty(self.resolve(field.ty));
            let (signed, conv) = match target.kind {
                tag::BOOL => (false, Conv2::Bool),
                tag::INT => (target.flags & tag::SIGNED != 0, Conv2::Int),
                tag::ENUM => {
                    let e = self.v.enums[at(target.a)];
                    let under = self.ty(self.resolve(e.underlying));
                    let named = self.enum_name[at(target.a)] != NONE;
                    (under.flags & tag::SIGNED != 0, if named { Conv2::Newtype } else { Conv2::Int })
                }
                _ => {
                    parts.push(Fail { cause: Unexpressed::Bitfield, part });
                    continue;
                }
            };
            let mut fname = String::new_in(self.alloc);
            if ident(&mut fname, self.v.name(field.name)).is_err() {
                parts.push(Fail { cause: Unexpressed::ReservedName, part });
                continue;
            }
            let mask = if width >= u64::from(bits) { u128::MAX } else { (1u128 << width) - 1 };
            let mask_text = mask & (u128::MAX >> (128 - bits));
            let raw = |s: &mut AString<'_>| {
                if signed {
                    format_into!(
                        s,
                        "((((self.__bitfield_{u} >> {shift}) << {}) as ::core::primitive::{signed_prim}) >> {})",
                        u64::from(bits) - width,
                        u64::from(bits) - width
                    );
                } else {
                    format_into!(s, "((self.__bitfield_{u} >> {shift}) & {mask_text:#x})");
                }
            };
            let bare = fname.as_str().trim_start_matches("r#");
            format_into!(accessors, "    pub fn {}(&self) -> {} {{\n        ", fname.as_str(), ty.as_str());
            match conv {
                Conv2::Bool => {
                    raw(accessors);
                    accessors.push_str(" != 0\n");
                }
                Conv2::Int => {
                    raw(accessors);
                    format_into!(accessors, " as {}\n", ty.as_str());
                }
                Conv2::Newtype => {
                    format_into!(accessors, "{}(", ty.as_str());
                    raw(accessors);
                    accessors.push_str(" as _)\n");
                }
            }
            accessors.push_str("    }\n");
            let input = match conv {
                Conv2::Newtype => "value.0",
                _ => "value",
            };
            format_into!(
                accessors,
                "    pub fn set_{bare}(&mut self, value: {}) {{\n        let v = ({input} as ::core::primitive::{prim}) & {mask_text:#x};\n        self.__bitfield_{u} = (self.__bitfield_{u} & !({mask_text:#x} << {shift})) | (v << {shift});\n    }}\n",
                ty.as_str()
            );
        }
        Ok(Form::Fields { exact })
    }

    /// The `repr` of record `k` in form `form`.
    fn repr(&self, k: u32, form: Form) -> (u64, u64) {
        let r = self.v.records[at(k)];
        match form {
            Form::Fields { .. } => (u64::from(r.pack), u64::from(r.align)),
            _ => (0, u64::from(r.align)),
        }
    }

    fn render_record(&mut self, k: u32, out: &mut AString<'_>, final_pass: bool) -> Result<Form, Fail> {
        let r = self.v.records[at(k)];
        let mut body = String::new_in(self.alloc);
        let mut asserts = String::new_in(self.alloc);
        let mut accessors = String::new_in(self.alloc);
        let mut parts: AVec<'_, Fail> = Vec::new_in(self.alloc);
        let form = self.record_body(k, &mut body, &mut asserts, &mut accessors, &mut parts)?;
        let mut name = String::new_in(self.alloc);
        self.rec_ident(&mut name, k)?;
        let union = r.kind == RecordKind::Union.byte();
        match form {
            Form::Opaque => {
                format_into!(
                    out,
                    "#[repr(C)]\npub struct {} {{\n    __opaque: [::core::primitive::u8; 0],\n    __marker: ::core::marker::PhantomData<(*mut ::core::primitive::u8, ::core::marker::PhantomPinned)>,\n}}\n",
                    name.as_str()
                );
            }
            Form::Fields { .. } | Form::Bytes => {
                let (pack, align) = self.repr(k, form);
                let natural = self.natural_align(k, form);
                out.push_str("#[repr(C");
                if pack > 0 && matches!(form, Form::Fields { .. }) {
                    format_into!(out, ", packed({pack})");
                } else if align > natural {
                    format_into!(out, ", align({align})");
                }
                out.push_str(")]\n#[derive(Clone, Copy)]\n");
                let keyword = if union && matches!(form, Form::Fields { .. }) { "union" } else { "struct" };
                format_into!(out, "pub {keyword} {} {{\n", name.as_str());
                out.push_str(body.as_str());
                out.push_str("}\n");
                if !accessors.as_str().is_empty() {
                    format_into!(out, "impl {} {{\n", name.as_str());
                    out.push_str(accessors.as_str());
                    out.push_str("}\n");
                }
                format_into!(
                    out,
                    "const _: () = {{\n    ::core::assert!(::core::mem::size_of::<{0}>() == {1});\n    ::core::assert!(::core::mem::align_of::<{0}>() == {2});\n",
                    name.as_str(),
                    r.size,
                    r.align
                );
                if matches!(form, Form::Fields { .. }) {
                    out.push_str(asserts.as_str());
                }
                out.push_str("};\n");
            }
        }
        if final_pass {
            for part in parts.iter() {
                self.header_finding(DeclKind::Record, r.name, r.ns, r.header, r.at, r.len, *part);
            }
        }
        Ok(form)
    }

    /// The alignment `repr(C)` alone gives record `k`'s rendered fields: the most of its
    /// fields' (packed: at most the pack); 1 for bytes.
    fn natural_align(&self, k: u32, form: Form) -> u64 {
        let r = self.v.records[at(k)];
        if !matches!(form, Form::Fields { .. }) {
            return 1;
        }
        let mut most = 1u64;
        for field in self.v.fields_of(&r) {
            let align = match field.bitfield {
                1 => {
                    let unit = self.v.units[at(field.unit)];
                    match unsigned_of(unit.size) {
                        Some(_) if unit.offset % unit.size == 0 => u64::from(unit.size),
                        _ => 1,
                    }
                }
                _ => {
                    let mut scratch = String::new_in(self.alloc);
                    match self.type_text(&mut scratch, field.ty, true, false, 0) {
                        Ok(()) => self.layout(field.ty, 0).map_or(1, |l| l.1),
                        Err(_) => 1,
                    }
                }
            };
            most = most.max(align);
        }
        match r.pack {
            0 => most,
            p => most.min(u64::from(p)),
        }
    }

    // ------------------------------------------------------------------- the rest

    fn render_enum(&self, k: u32, out: &mut AString<'_>) -> Result<(), Fail> {
        let e = self.v.enums[at(k)];
        let name = self.enum_name[at(k)];
        if e.underlying == NONE {
            // Declared with no underlying type known: only pointed at.
            if name == NONE {
                return Err(fail(Unexpressed::IncompleteByValue));
            }
            out.push_str("#[repr(C)]\npub struct ");
            ident(out, self.v.name(name))?;
            out.push_str(
                " {\n    __opaque: [::core::primitive::u8; 0],\n    __marker: ::core::marker::PhantomData<(*mut ::core::primitive::u8, ::core::marker::PhantomPinned)>,\n}\n",
            );
            return Ok(());
        }
        let under = self.ty(self.resolve(e.underlying));
        let mut under_text = String::new_in(self.alloc);
        self.type_text(&mut under_text, e.underlying, true, false, 0)?;
        let consts = self.v.consts_of(&e);
        if name == NONE {
            for c in consts {
                if !self.request.selects(self.v.name(c.name)) {
                    continue;
                }
                out.push_str("pub const ");
                ident(out, self.v.name(c.name))?;
                format_into!(out, ": {} = ", under_text.as_str());
                int_literal(out, c.value, under);
                out.push_str(";\n");
            }
            return Ok(());
        }
        let mut n = String::new_in(self.alloc);
        ident(&mut n, self.v.name(name))?;
        format_into!(
            out,
            "#[repr(transparent)]\n#[derive(Clone, Copy, PartialEq, Eq, Hash)]\npub struct {}(pub {});\n",
            n.as_str(),
            under_text.as_str()
        );
        if !consts.is_empty() {
            format_into!(out, "impl {} {{\n", n.as_str());
            for c in consts {
                out.push_str("    pub const ");
                ident(out, self.v.name(c.name))?;
                format_into!(out, ": {0} = {0}(", n.as_str());
                int_literal(out, c.value, under);
                out.push_str(");\n");
            }
            out.push_str("}\n");
        }
        format_into!(out, "const _: () = ::core::assert!(::core::mem::size_of::<{}>() == {});\n", n.as_str(), under.a);
        Ok(())
    }

    fn render_typedef(&self, k: u32, out: &mut AString<'_>) -> Result<(), Fail> {
        let t = self.v.typedefs[at(k)];
        let target = self.resolve(t.ty);
        let mut text = String::new_in(self.alloc);
        match self.ty(target).kind {
            tag::FUNCTION => return Err(fail(Unexpressed::FunctionTypedef)),
            tag::VOID => text.push_str("::core::ffi::c_void"),
            _ => self.type_text(&mut text, t.ty, true, false, 0)?,
        }
        out.push_str("pub type ");
        ident(out, self.v.name(t.name))?;
        format_into!(out, " = {};\n", text.as_str());
        Ok(())
    }

    fn symbol(&self, out: &mut AString<'_>, name: u32, symbol: u32, literal: bool) {
        if symbol != name || literal {
            let prefix = if literal { "\\u{1}" } else { "" };
            format_into!(out, "    #[link_name = \"{prefix}{}\"]\n", self.v.name(symbol));
        }
    }

    fn render_fn(&self, k: usize, out: &mut AString<'_>) -> Result<Conv, Fail> {
        let f = self.v.fns[k];
        let ty = self.ty(f.ty);
        if ty.kind == tag::UNEXPRESSIBLE {
            return Err(fail(Unexpressed::from_byte(ty.extra).expect("checked by open")));
        }
        if ty.flags & tag::PROTOTYPED == 0 {
            return Err(fail(Unexpressed::Unprototyped));
        }
        let conv = Conv::from_byte(ty.extra).expect("checked by open");
        abi_name(conv)?;
        let variadic = ty.flags & tag::VARIADIC != 0;
        if variadic && conv != Conv::C {
            return Err(fail(Unexpressed::VariadicConvention));
        }
        let attrs = FnAttrs(f.attrs);
        if attrs.has(FnAttrs::DEPRECATED) {
            out.push_str("    #[deprecated]\n");
        }
        match fn_is_local(&f) {
            // A body of the headers': its thunk in the import's unit (kept only with a unit).
            true => self.unit_symbol(out, f.name)?,
            false => self.symbol(out, f.name, f.symbol, attrs.has(FnAttrs::SYMBOL_LITERAL)),
        }
        out.push_str("    pub unsafe fn ");
        ident(out, self.v.name(f.name))?;
        out.push_str("(");
        let names = self.v.param_names(&f);
        for (j, param) in self.v.params_of(&ty).iter().enumerate() {
            if j > 0 {
                out.push_str(", ");
            }
            let mut pn = String::new_in(self.alloc);
            match names.get(j).copied().unwrap_or(NONE) {
                NONE => pn.push_str("_"),
                n => {
                    if ident(&mut pn, self.v.name(n)).is_err() {
                        pn.clear();
                        pn.push_str("_");
                    }
                }
            }
            format_into!(out, "{}: ", pn.as_str());
            self.type_text(out, *param, true, true, 0).map_err(|e| Fail { cause: e.cause, part: narrow(j).unwrap_or(NONE) })?;
        }
        if variadic {
            out.push_str(if ty.c == 0 { "..." } else { ", ..." });
        }
        out.push_str(")");
        self.result(out, ty.a, attrs.has(FnAttrs::NORETURN), 0)?;
        out.push_str(";\n");
        Ok(conv)
    }

    fn render_var(&self, k: usize, out: &mut AString<'_>) -> Result<(), Fail> {
        let var = self.v.vars[k];
        let attrs = VarAttrs(var.attrs);
        if attrs.has(VarAttrs::THREAD_LOCAL) {
            return Err(fail(Unexpressed::ThreadLocal));
        }
        if var_is_local(&var) {
            // A header-local variable: its one instance is the unit's, reached by its address.
            self.unit_symbol(out, var.name)?;
            out.push_str("    pub safe fn ");
            ident(out, self.v.name(var.name))?;
            out.push_str(if self.is_const(var.ty) { "() -> *const " } else { "() -> *mut " });
            self.type_text(out, var.ty, true, false, 0)?;
            out.push_str(";\n");
            return Ok(());
        }
        self.symbol(out, var.name, var.symbol, attrs.has(VarAttrs::SYMBOL_LITERAL));
        // An item of an `unsafe extern` block is unsafe unless marked `safe`; `static mut` takes
        // no marker.
        out.push_str(if self.is_const(var.ty) { "    pub unsafe static " } else { "    pub static mut " });
        ident(out, self.v.name(var.name))?;
        out.push_str(": ");
        self.type_text(out, var.ty, true, false, 0)?;
        out.push_str(";\n");
        Ok(())
    }

    /// `#[link_name]` of the unit's symbol for `name` (the unit is there: a declaration bound
    /// to it is dropped without one).
    fn unit_symbol(&self, out: &mut AString<'_>, name: u32) -> Result<(), Fail> {
        let Some(unit) = self.unit else { return Err(fail(Unexpressed::InlineFunction)) };
        out.push_str("    #[link_name = \"");
        unit.symbol(self.v.name(name), out);
        out.push_str("\"]\n");
        Ok(())
    }

    /// An expression of the address of variable `k`, as `*const T` or `*mut T`: the static's
    /// raw address, or the unit's accessor for a header-local one.
    fn address_of(&self, out: &mut AString<'_>, k: u32) -> Result<(), Fail> {
        if self.vars[at(k)] != Status::Keep {
            return Err(fail(Unexpressed::DependsOnDropped));
        }
        let var = self.v.vars[at(k)];
        match var_is_local(&var) {
            true => {
                ident(out, self.v.name(var.name))?;
                out.push_str("()");
            }
            false => {
                out.push_str(if self.is_const(var.ty) { "&raw const " } else { "&raw mut " });
                ident(out, self.v.name(var.name))?;
            }
        }
        Ok(())
    }

    /// A slot macro: entry `m.value` of the table `m.var`, as the macro's pointer type `m.ty`.
    fn render_slot(&self, m: &MacroRow, name: &str, out: &mut AString<'_>) -> Result<(), Fail> {
        let var = self.v.vars[at(m.var)];
        let table_name = self.v.name(var.name);
        let table = self.ty(self.resolve(var.ty));
        let elem = match table.kind {
            tag::POINTER | tag::ARRAY => table.a,
            _ => return Err(fail(Unexpressed::MacroNotConstant)),
        };
        // An entry is a pointer (C's `void *` tables); the macro casts it.
        if self.ty(self.resolve(elem)).kind != tag::POINTER {
            return Err(fail(Unexpressed::MacroNotConstant));
        }
        let mut load = String::new_in(self.alloc);
        load.push_str("unsafe { (");
        self.address_of(&mut load, m.var)?;
        match table.kind {
            tag::POINTER => load.push_str(").read() }"),
            _ => {
                load.push_str(").cast::<");
                self.type_text(&mut load, elem, true, false, 0)?;
                load.push_str(">() }");
            }
        }
        let mut cast = String::new_in(self.alloc);
        self.type_text(&mut cast, m.ty, true, false, 0)?;
        let pointee = self.resolve(self.ty(self.resolve(m.ty)).a);
        let function = self.ty(pointee).kind == tag::FUNCTION;
        let index = m.value;
        let reads = match (m.deref, function) {
            (1, false) => " The macro reads through the cast pointer (`*(T *)table[i]`): this is that pointer.",
            _ => "",
        };
        format_into!(
            out,
            "/// `{name}`: entry {index} of the table `{table_name}`, as the header's macro reads it.{reads}\n///\n/// # Safety\n///\n/// `{table_name}` is null until the library's import call has filled it (numpy's\n/// `import_array()` is one), and only the header says entry {index} has this type.\n"
        );
        // A pointer table is checked, naming it; an array's address is never null.
        let mut check = String::new_in(self.alloc);
        if table.kind == tag::POINTER {
            format_into!(
                &mut check,
                "    ::core::assert!(!table.is_null(), \"`{table_name}` is null: the library's import call has not run\");\n"
            );
        }
        let null_check = check.as_str();
        if !function {
            format_into!(
                out,
                "#[inline]\npub unsafe fn {name}() -> {} {{\n    let table = {};\n{null_check}    (unsafe {{ table.add({index}).read() }}) as {}\n}}\n",
                cast.as_str(),
                load.as_str(),
                cast.as_str()
            );
            return Ok(());
        }
        let fty = self.ty(pointee);
        if fty.flags & tag::PROTOTYPED == 0 {
            return Err(fail(Unexpressed::Unprototyped));
        }
        if fty.flags & tag::VARIADIC != 0 {
            // A Rust function cannot pass `...` on: the entry itself, a function pointer.
            format_into!(
                out,
                "#[inline]\npub unsafe fn {name}() -> {} {{\n    let table = {};\n{null_check}    unsafe {{ ::core::mem::transmute(table.add({index}).read()) }}\n}}\n",
                cast.as_str(),
                load.as_str()
            );
            return Ok(());
        }
        let mut params = String::new_in(self.alloc);
        let mut args = String::new_in(self.alloc);
        for (j, param) in self.v.params_of(&fty).iter().enumerate() {
            if j > 0 {
                params.push_str(", ");
                args.push_str(", ");
            }
            format_into!(&mut params, "a{j}: ");
            format_into!(&mut args, "a{j}");
            self.type_text(&mut params, *param, true, true, 0).map_err(|e| Fail { cause: e.cause, part: narrow(j).unwrap_or(NONE) })?;
        }
        let mut result = String::new_in(self.alloc);
        self.result(&mut result, fty.a, false, 0)?;
        format_into!(
            out,
            "#[inline]\npub unsafe fn {name}({}){} {{\n    let table = {};\n{null_check}    let entry: {} = unsafe {{ ::core::mem::transmute(table.add({index}).read()) }};\n    let ::core::option::Option::Some(f) = entry else {{\n        ::core::panic!(\"`{table_name}[{index}]` is null: the library's table has no entry there\")\n    }};\n    unsafe {{ f({}) }}\n}}\n",
            params.as_str(),
            result.as_str(),
            load.as_str(),
            cast.as_str(),
            args.as_str()
        );
        Ok(())
    }

    /// An integer constant's literal of type `k`: through typedefs, `_Bool` as `true`/`false`,
    /// an enum's as its newtype of the underlying integer (an anonymous one's as the integer).
    fn int_value(&self, out: &mut AString<'_>, bits: u64, k: u32) -> Result<(), Fail> {
        let row = self.ty(self.resolve(k));
        match row.kind {
            tag::BOOL => out.push_str(if bits != 0 { "true" } else { "false" }),
            tag::INT => int_literal(out, bits, row),
            tag::ENUM => {
                let under = match self.v.enums[at(row.a)].underlying {
                    NONE => return Err(fail(Unexpressed::IncompleteByValue)),
                    u => self.ty(self.resolve(u)),
                };
                if under.kind != tag::INT {
                    return Err(fail(Unexpressed::MacroNotConstant));
                }
                match self.enum_name[at(row.a)] {
                    NONE => int_literal(out, bits, under),
                    name => {
                        ident(out, self.v.name(name))?;
                        out.push_str("(");
                        int_literal(out, bits, under);
                        out.push_str(")");
                    }
                }
            }
            _ => return Err(fail(Unexpressed::MacroNotConstant)),
        }
        Ok(())
    }

    fn render_macro(&self, m: &MacroRow, out: &mut AString<'_>) -> Result<(), Fail> {
        let ty = self.ty(self.resolve(m.ty));
        let mut name = String::new_in(self.alloc);
        ident(&mut name, self.v.name(m.name))?;
        match m.kind {
            value::INT => {
                let mut text = String::new_in(self.alloc);
                self.type_text(&mut text, m.ty, true, false, 0)?;
                let mut literal = String::new_in(self.alloc);
                self.int_value(&mut literal, m.value, m.ty)?;
                format_into!(out, "pub const {}: {} = {};\n", name.as_str(), text.as_str(), literal.as_str());
            }
            value::SLOT => self.render_slot(m, name.as_str(), out)?,
            value::ADDRESS_OF => {
                let mut text = String::new_in(self.alloc);
                self.type_text(&mut text, m.ty, true, false, 0)?;
                let mut address = String::new_in(self.alloc);
                self.address_of(&mut address, m.var)?;
                let mutable = if self.is_const(m.ty) { "const" } else { "mut" };
                format_into!(
                    out,
                    "/// `{}`: the address of `{}`.\n#[inline]\npub fn {}() -> *{mutable} {} {{\n    {}\n}}\n",
                    name.as_str(),
                    self.v.name(self.v.vars[at(m.var)].name),
                    name.as_str(),
                    text.as_str(),
                    address.as_str()
                );
            }
            value::FLOAT => {
                let (prim, hex) = match ty.a {
                    4 => ("f32", m.value & 0xffff_ffff),
                    8 => ("f64", m.value),
                    _ => return Err(fail(Unexpressed::LongDouble)),
                };
                format_into!(
                    out,
                    "pub const {}: ::core::primitive::{prim} = ::core::primitive::{prim}::from_bits({hex:#x});\n",
                    name.as_str()
                );
            }
            _ => {
                let bytes = self.v.string_of(m);
                if bytes.contains(&0) {
                    return Err(fail(Unexpressed::StringWithNul));
                }
                format_into!(out, "pub const {}: &::core::ffi::CStr = c\"", name.as_str());
                for b in bytes {
                    match *b {
                        b'"' => out.push_str("\\\""),
                        b'\\' => out.push_str("\\\\"),
                        printable @ 0x20..=0x7e => out.push(char::from(printable)),
                        other => format_into!(out, "\\x{other:02x}"),
                    }
                }
                out.push_str("\";\n");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Conv2 {
    Bool,
    Int,
    Newtype,
}

/// The integer `bits` of type `ty` as a Rust literal: sign-extended for a signed type, masked
/// to the type's size otherwise.
fn int_literal(out: &mut AString<'_>, bits: u64, ty: TypeRow) {
    let size_bits = (ty.a * 8).min(64);
    if ty.flags & tag::SIGNED != 0 {
        let shift = 64 - size_bits;
        let value = i64::from_ne_bytes((bits << shift).to_ne_bytes()) >> shift;
        format_into!(out, "{value}");
    } else {
        let mask = if size_bits >= 64 { u64::MAX } else { (1u64 << size_bits) - 1 };
        format_into!(out, "{}", bits & mask);
    }
}

/// Whether a declaration's first part is C++: a namespace, a method, an overload.
fn cpp_cause(ns: u32) -> Option<Unexpressed> {
    (ns != NONE).then_some(Unexpressed::CppNamespace)
}

/// Whether a function's code is the import's unit's (internal or inline linkage).
fn fn_is_local(f: &FnRow) -> bool {
    Linkage::from_byte(f.linkage).is_some_and(Linkage::is_local)
}

/// Whether a variable is header-local (internal linkage): the unit's.
fn var_is_local(v: &VarRow) -> bool {
    Linkage::from_byte(v.linkage).is_some_and(Linkage::is_local)
}

/// `t` as Rust text: its pointers, then a `core::ffi` scalar or the header's type of the name.
fn macro_type_text(out: &mut AString<'_>, t: &MacroType<'_>) -> Result<(), Fail> {
    for j in 0..t.depth {
        out.push_str(if t.muts & (1 << j) != 0 { "*mut " } else { "*const " });
    }
    match t.is_scalar() {
        true => {
            out.push_str("::core::ffi::");
            out.push_str(t.name);
            Ok(())
        }
        false => ident(out, t.name),
    }
}

/// Whether two functions are members of one overload set (the record sorts them together).
fn same_set(a: &FnRow, b: &FnRow) -> bool {
    a.ns == b.ns && a.name == b.name
}

type Names<'v, 'a> = Map<&'v str, (), FixedState, Alloc<'a>>;

/// Claims `name` in one Rust namespace: the first claim in the record's order keeps it.
fn name_status<'v>(map: &mut Names<'v, '_>, name: &'v str) -> Status {
    if UNSPELLABLE.contains(&name) {
        return Status::Drop(Unexpressed::ReservedName);
    }
    if map.get(&name).is_some() {
        return Status::Drop(Unexpressed::NameClash);
    }
    map.insert(name, ());
    Status::Keep
}

/// The records, enums and typedefs reached from the selected declarations.
struct Reach<'v, 'a> {
    v: &'v DeclsView<'v>,
    records: AVec<'a, bool>,
    enums: AVec<'a, bool>,
    typedefs: AVec<'a, bool>,
    work: AVec<'a, u32>,
}

impl<'v, 'a> Reach<'v, 'a> {
    fn new(v: &'v DeclsView<'v>, alloc: Alloc<'a>) -> Reach<'v, 'a> {
        let flags = |n: usize| {
            let mut out: AVec<'a, bool> = Vec::with_capacity_in(n, alloc);
            out.resize(n, false);
            out
        };
        Reach {
            v,
            records: flags(v.records.len()),
            enums: flags(v.enums.len()),
            typedefs: flags(v.typedefs.len()),
            work: Vec::new_in(alloc),
        }
    }

    fn record(&mut self, k: u32) {
        if let Some(seen) = self.records.get_mut(at(k))
            && !*seen
        {
            *seen = true;
            self.work.push(k);
        }
    }

    /// Marks what type `k` names. The record's types refer only to earlier types (checked by
    /// open), so the walk ends; records are walked from the work list, once each.
    fn ty(&mut self, k: u32, depth: u32) {
        if depth > MOST_DEPTH {
            return;
        }
        let row = self.v.types[at(k)];
        match row.kind {
            tag::POINTER | tag::REFERENCE | tag::ARRAY | tag::VA_LIST => self.ty(row.a, depth + 1),
            tag::FUNCTION => {
                self.ty(row.a, depth + 1);
                for param in self.v.params_of(&row) {
                    self.ty(*param, depth + 1);
                }
            }
            tag::RECORD => self.record(row.a),
            tag::ENUM => {
                self.enums[at(row.a)] = true;
                let under = self.v.enums[at(row.a)].underlying;
                if under != NONE {
                    self.ty(under, depth + 1);
                }
            }
            tag::TYPEDEF => {
                self.typedefs[at(row.a)] = true;
                self.ty(self.v.typedefs[at(row.a)].ty, depth + 1);
            }
            _ => {}
        }
    }

    fn drain(&mut self) {
        while let Some(k) = self.work.pop() {
            let r = self.v.records[at(k)];
            for field in self.v.fields_of(&r) {
                self.ty(field.ty, 0);
            }
        }
    }
}

/// A static assert per C scalar the record uses, that `core`'s type has the record's size and
/// alignment, and of the pointer's size and `char`'s sign: a record for one target does not
/// compile for another.
fn scalar_asserts(r: &R<'_, '_, '_>, out: &mut AString<'_>) {
    let mut seen = [false; 16];
    out.push_str("const _: () = {\n");
    for row in r.v.types.iter() {
        if row.kind != tag::INT {
            continue;
        }
        let Some(kind) = IntKind::from_byte(row.extra) else { continue };
        let Some(slot) = seen.get_mut(usize::from(kind.byte())) else { continue };
        if *slot {
            continue;
        }
        *slot = true;
        let name = c_int_name(kind);
        format_into!(
            out,
            "    ::core::assert!(::core::mem::size_of::<{name}>() == {});\n    ::core::assert!(::core::mem::align_of::<{name}>() == {});\n",
            row.a,
            row.b
        );
    }
    let abi = r.v.abi;
    format_into!(
        out,
        "    ::core::assert!(::core::mem::size_of::<*const ::core::primitive::u8>() == {});\n    ::core::assert!((::core::ffi::c_char::MIN != 0) == {});\n}};\n",
        abi.pointer_size,
        abi.char_signed == 1
    );
}

/// The Rust module of `record` for `request`, in `alloc`. `unit` is the import's own C unit
/// ([`UnitKey::of`]), which the functions the headers define, their header-local variables and
/// the request's `macro_fn`s are bound to; `None` renders without one (each such function and
/// variable a finding, a `macro_fn` refused). The key of what this returns is
/// [`crate::key::RenderKey::of`] of the same three.
pub fn render<'a>(
    record: &[u8],
    request: &Request<'_>,
    unit: Option<UnitKey>,
    alloc: Alloc<'a>,
) -> Result<Rendered<'a>, RenderError> {
    let v = DeclsView::open(record).map_err(RenderError::View)?;
    let fill = |n: usize, s: Status| {
        let mut out: AVec<'a, Status> = Vec::with_capacity_in(n, alloc);
        out.resize(n, s);
        out
    };
    let mut r = R {
        v,
        request,
        rec: fill(v.records.len(), Status::Keep),
        form: {
            let mut f: AVec<'a, Form> = Vec::with_capacity_in(v.records.len(), alloc);
            f.resize(v.records.len(), Form::Opaque);
            f
        },
        rec_name: Vec::new_in(alloc),
        enums: fill(v.enums.len(), Status::Keep),
        enum_name: Vec::new_in(alloc),
        typedefs: fill(v.typedefs.len(), Status::Keep),
        alias: Vec::new_in(alloc),
        fns: fill(v.fns.len(), Status::Keep),
        vars: fill(v.vars.len(), Status::Keep),
        macros: fill(v.macros.len(), Status::Keep),
        findings: Vec::new_in(alloc),
        unit,
        alloc,
    };
    // Names: a record's or enum's own, else the first typedef that names it directly.
    for row in v.records.iter() {
        r.rec_name.push(row.name);
    }
    for row in v.enums.iter() {
        r.enum_name.push(row.name);
    }
    for t in v.typedefs.iter() {
        let target = v.types[at(t.ty)];
        let direct = target.quals == 0;
        let mut alias = false;
        match target.kind {
            tag::RECORD if direct => {
                if r.rec_name[at(target.a)] == NONE {
                    r.rec_name[at(target.a)] = t.name;
                }
                alias = r.rec_name[at(target.a)] == t.name;
            }
            tag::ENUM if direct => {
                if r.enum_name[at(target.a)] == NONE {
                    r.enum_name[at(target.a)] = t.name;
                }
                alias = r.enum_name[at(target.a)] == t.name;
            }
            _ => {}
        }
        r.alias.push(alias);
    }

    // Local causes: C++ parts, attributes, overloads.
    for (k, row) in v.records.iter().enumerate() {
        let cause = cpp_cause(row.ns)
            .or((row.bases_len > 0).then_some(Unexpressed::CppBases))
            .or((row.relocation != Relocation::TriviallyCopyable.byte()).then_some(Unexpressed::CppRelocation));
        if let Some(cause) = cause {
            r.rec[k] = Status::Drop(cause);
        }
    }
    for (k, row) in v.enums.iter().enumerate() {
        if let Some(cause) = cpp_cause(row.ns) {
            r.enums[k] = Status::Drop(cause);
        }
    }
    for (k, row) in v.typedefs.iter().enumerate() {
        if let Some(cause) = cpp_cause(row.ns) {
            r.typedefs[k] = Status::Drop(cause);
        }
    }
    for (k, row) in v.fns.iter().enumerate() {
        let attrs = FnAttrs(row.attrs);
        let overloaded = (k > 0 && same_set(&v.fns[k - 1], row)) || v.fns.get(k + 1).is_some_and(|n| same_set(n, row));
        let cause = cpp_cause(row.ns)
            .or((row.method != MethodKind::Free.byte()).then_some(Unexpressed::CppMethod))
            .or(overloaded.then_some(Unexpressed::CppOverloads))
            .or(attrs.has(FnAttrs::MAY_THROW).then_some(Unexpressed::CppMayThrow))
            .or(attrs.has(FnAttrs::RETURNS_TWICE).then_some(Unexpressed::ReturnsTwice));
        // A function of internal or inline linkage renders only as its body's thunk: with a
        // body in the headers, bodies asked for, a unit to hold them, and no `...` to pass on.
        let fn_ty = v.types[at(row.ty)];
        let thunk = row.body == 1
            && request.inline_bodies
            && unit.is_some()
            && !(fn_ty.kind == tag::FUNCTION && fn_ty.flags & tag::VARIADIC != 0);
        let cause = cause.or((fn_is_local(row) && !thunk).then_some(Unexpressed::InlineFunction));
        if let Some(cause) = cause {
            r.fns[k] = Status::Drop(cause);
        }
    }
    for (k, row) in v.vars.iter().enumerate() {
        let cause = cpp_cause(row.ns).or((var_is_local(row) && unit.is_none()).then_some(Unexpressed::HeaderVariable));
        if let Some(cause) = cause {
            r.vars[k] = Status::Drop(cause);
        }
    }

    // Selection: functions, data and constants by the patterns; a type when it is selected
    // itself or reached from a kept declaration.
    let selects = |name: u32| name != NONE && request.selects(v.name(name));
    for (k, row) in v.fns.iter().enumerate() {
        if r.fns[k] == Status::Keep && !selects(row.name) {
            r.fns[k] = Status::Drop(Unexpressed::NotSelected);
        }
    }
    for (k, row) in v.vars.iter().enumerate() {
        if r.vars[k] == Status::Keep && !selects(row.name) {
            r.vars[k] = Status::Drop(Unexpressed::NotSelected);
        }
    }
    for (k, row) in v.macros.iter().enumerate() {
        if !selects(row.name) {
            r.macros[k] = Status::Drop(Unexpressed::NotSelected);
        }
    }
    // A selected macro that names a variable keeps it: `allow "Py_None";` keeps
    // `_Py_NoneStruct`, as a selected function keeps the types it names.
    for (k, row) in v.macros.iter().enumerate() {
        if r.macros[k] == Status::Keep && row.var != NONE && r.vars[at(row.var)] == Status::Drop(Unexpressed::NotSelected) {
            r.vars[at(row.var)] = Status::Keep;
        }
    }
    // Every `macro_fn` is asked for by name: refused unless the headers have the function-like
    // macro, and no function of its name (which would be its meaning, and renders).
    let mut fn_names: Names<'_, 'a> = Map::new(MapOpts { hasher: FixedState, alloc, capacity: None });
    for row in v.fns.iter() {
        if row.ns == NONE {
            fn_names.insert(v.name(row.name), ());
        }
    }
    for (j, f) in request.macro_fns.iter().enumerate() {
        let index = narrow(j).ok_or(RenderError::TooLarge)?;
        let refuse = |fault: MacroFnFault| RenderError::MacroFn { index, fault };
        if unit.is_none() {
            return Err(refuse(MacroFnFault::NoUnit));
        }
        if fn_names.get(&f.name).is_some() {
            return Err(refuse(MacroFnFault::IsAFunction));
        }
        let row = v.unexpressed.iter().find(|u| u.kind == DeclKind::Macro.byte() && u.ns == NONE && v.name(u.name) == f.name);
        match row.map(|u| Unexpressed::from_byte(u.cause).expect("checked by open")) {
            None => return Err(refuse(MacroFnFault::NoSuchMacro)),
            Some(Unexpressed::FunctionLikeMacro) => {}
            Some(cause) => return Err(refuse(MacroFnFault::NotAFunction(cause))),
        }
    }
    let mut reach = Reach::new(&v, alloc);
    // The header's types a `macro_fn` names are reached as a function's are.
    for f in request.macro_fns.iter() {
        let named = request.params_of(f).iter().map(|p| p.ty).chain(f.result);
        for t in named.filter(|t| !t.is_scalar()) {
            for (k, row) in v.typedefs.iter().enumerate() {
                if v.name(row.name) == t.name {
                    reach.ty(row.ty, 0);
                    reach.typedefs[k] = true;
                }
            }
            for k in 0..v.records.len() {
                if r.rec_name[k] != NONE && v.name(r.rec_name[k]) == t.name {
                    reach.record(narrow(k).ok_or(RenderError::TooLarge)?);
                }
            }
            for k in 0..v.enums.len() {
                if r.enum_name[k] != NONE && v.name(r.enum_name[k]) == t.name {
                    reach.enums[k] = true;
                }
            }
        }
    }
    // An anonymous record with no typedef naming it is a member, reached from its parent.
    for k in 0..v.records.len() {
        if selects(r.rec_name[k]) {
            reach.record(narrow(k).ok_or(RenderError::TooLarge)?);
        }
    }
    for (k, _) in v.enums.iter().enumerate() {
        let name = r.enum_name[k];
        let anonymous_selected = name == NONE && v.consts_of(&v.enums[k]).iter().any(|c| selects(c.name));
        if selects(name) || anonymous_selected {
            reach.enums[k] = true;
        }
    }
    for (k, row) in v.typedefs.iter().enumerate() {
        if selects(row.name) {
            reach.ty(row.ty, 0);
            reach.typedefs[k] = true;
        }
    }
    for (k, row) in v.fns.iter().enumerate() {
        if r.fns[k] == Status::Keep {
            reach.ty(row.ty, 0);
        }
    }
    for (k, row) in v.vars.iter().enumerate() {
        if r.vars[k] == Status::Keep {
            reach.ty(row.ty, 0);
        }
    }
    for (k, row) in v.macros.iter().enumerate() {
        if r.macros[k] == Status::Keep {
            reach.ty(row.ty, 0);
        }
    }
    reach.drain();
    for k in 0..v.records.len() {
        if !reach.records[k] && r.rec[k] == Status::Keep {
            r.rec[k] = Status::Drop(Unexpressed::NotSelected);
        }
    }
    for k in 0..v.enums.len() {
        if !reach.enums[k] && r.enums[k] == Status::Keep {
            r.enums[k] = Status::Drop(Unexpressed::NotSelected);
        }
    }
    for k in 0..v.typedefs.len() {
        if !reach.typedefs[k] && r.typedefs[k] == Status::Keep {
            r.typedefs[k] = Status::Drop(Unexpressed::NotSelected);
        }
    }

    // Names: one Rust name per namespace (types; values, where an enum's newtype is too).
    let mut types: Names<'_, 'a> = Map::new(MapOpts { hasher: FixedState, alloc, capacity: None });
    let mut values: Names<'_, 'a> = Map::new(MapOpts { hasher: FixedState, alloc, capacity: None });
    for k in 0..v.records.len() {
        if r.rec[k] != Status::Keep || r.rec_name[k] == NONE {
            continue;
        }
        let name = v.name(r.rec_name[k]);
        r.rec[k] = name_status(&mut types, name);
    }
    for k in 0..v.enums.len() {
        if r.enums[k] != Status::Keep {
            continue;
        }
        match r.enum_name[k] {
            NONE => {
                for c in v.consts_of(&v.enums[k]) {
                    if selects(c.name) && name_status(&mut values, v.name(c.name)) != Status::Keep {
                        r.enums[k] = Status::Drop(Unexpressed::NameClash);
                    }
                }
            }
            name => {
                let text = v.name(name);
                let status = name_status(&mut types, text);
                r.enums[k] = match status {
                    Status::Keep => name_status(&mut values, text),
                    other => other,
                };
            }
        }
    }
    for k in 0..v.typedefs.len() {
        if r.typedefs[k] != Status::Keep || r.alias[k] {
            continue;
        }
        r.typedefs[k] = name_status(&mut types, v.name(v.typedefs[k].name));
    }
    for k in 0..v.fns.len() {
        if r.fns[k] == Status::Keep {
            r.fns[k] = name_status(&mut values, v.name(v.fns[k].name));
        }
    }
    for k in 0..v.vars.len() {
        if r.vars[k] == Status::Keep {
            r.vars[k] = name_status(&mut values, v.name(v.vars[k].name));
        }
    }
    for k in 0..v.macros.len() {
        if r.macros[k] == Status::Keep {
            r.macros[k] = name_status(&mut values, v.name(v.macros[k].name));
        }
    }
    for (j, f) in request.macro_fns.iter().enumerate() {
        if name_status(&mut values, f.name) != Status::Keep {
            let index = narrow(j).ok_or(RenderError::TooLarge)?;
            return Err(RenderError::MacroFn { index, fault: MacroFnFault::NameClash });
        }
    }

    // Until nothing more drops: render each kept declaration into scratch; one that fails drops
    // with its cause, and those that use it drop in a later round.
    let total = v.records.len() + v.enums.len() + v.typedefs.len() + v.fns.len() + v.vars.len() + v.macros.len();
    for _round in 0..=total {
        let mut changed = false;
        for k in 0..v.records.len() {
            if r.rec[k] != Status::Keep {
                continue;
            }
            let mut scratch = String::new_in(alloc);
            let id = narrow(k).ok_or(RenderError::TooLarge)?;
            match r.render_record(id, &mut scratch, false) {
                Ok(form) => {
                    if r.form[k] != form {
                        r.form[k] = form;
                        changed = true;
                    }
                }
                Err(f) => {
                    r.rec[k] = Status::Drop(f.cause);
                    changed = true;
                }
            }
        }
        for k in 0..v.enums.len() {
            if r.enums[k] != Status::Keep {
                continue;
            }
            let mut scratch = String::new_in(alloc);
            if let Err(f) = r.render_enum(narrow(k).ok_or(RenderError::TooLarge)?, &mut scratch) {
                r.enums[k] = Status::Drop(f.cause);
                changed = true;
            }
        }
        for k in 0..v.typedefs.len() {
            if r.typedefs[k] != Status::Keep || r.alias[k] {
                continue;
            }
            let mut scratch = String::new_in(alloc);
            if let Err(f) = r.render_typedef(narrow(k).ok_or(RenderError::TooLarge)?, &mut scratch) {
                r.typedefs[k] = Status::Drop(f.cause);
                changed = true;
            }
        }
        for k in 0..v.fns.len() {
            if r.fns[k] != Status::Keep {
                continue;
            }
            let mut scratch = String::new_in(alloc);
            if let Err(f) = r.render_fn(k, &mut scratch) {
                r.fns[k] = Status::Drop(f.cause);
                changed = true;
            }
        }
        for k in 0..v.vars.len() {
            if r.vars[k] != Status::Keep {
                continue;
            }
            let mut scratch = String::new_in(alloc);
            if let Err(f) = r.render_var(k, &mut scratch) {
                r.vars[k] = Status::Drop(f.cause);
                changed = true;
            }
        }
        for k in 0..v.macros.len() {
            if r.macros[k] != Status::Keep {
                continue;
            }
            let mut scratch = String::new_in(alloc);
            if let Err(f) = r.render_macro(&v.macros[k], &mut scratch) {
                r.macros[k] = Status::Drop(f.cause);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Each `macro_fn`'s signature, now that what renders is settled.
    let mut wrappers = String::new_in(alloc);
    for (j, f) in request.macro_fns.iter().enumerate() {
        let index = narrow(j).ok_or(RenderError::TooLarge)?;
        let unknown = |part: u32| RenderError::MacroFn { index, fault: MacroFnFault::UnknownType { part } };
        let params = request.params_of(f);
        let typed = params.iter().map(|p| p.ty).chain(f.result).enumerate();
        for (part, t) in typed {
            let part = match part < params.len() {
                true => narrow(part).ok_or(RenderError::TooLarge)?,
                false => NONE,
            };
            if !t.is_scalar() && !r.renders_type(t.name, t.depth == 0) {
                return Err(unknown(part));
            }
        }
        let text = |out: &mut AString<'_>, t: &MacroType<'_>, part: u32| macro_type_text(out, t).map_err(|_| unknown(part));
        let symbol = unit.expect("refused above without a unit");
        wrappers.push_str("    #[link_name = \"");
        symbol.symbol(f.name, &mut wrappers);
        wrappers.push_str("\"]\n    pub unsafe fn ");
        ident(&mut wrappers, f.name).map_err(|_| unknown(NONE))?;
        wrappers.push_str("(");
        for (k, p) in params.iter().enumerate() {
            if k > 0 {
                wrappers.push_str(", ");
            }
            let part = narrow(k).ok_or(RenderError::TooLarge)?;
            if ident(&mut wrappers, p.name).is_err() {
                wrappers.push_str("_");
            }
            wrappers.push_str(": ");
            text(&mut wrappers, &p.ty, part)?;
        }
        wrappers.push_str(")");
        if let Some(t) = &f.result {
            wrappers.push_str(" -> ");
            text(&mut wrappers, t, NONE)?;
        }
        wrappers.push_str(";\n");
    }

    // The front end's own findings, then each dropped declaration's, in the record's order. A
    // macro row of a name a function has is no finding: the function is the macro's meaning
    // (cpython object.h:234-235, `Py_Is` both); nor is one a `macro_fn` rendered.
    for row in v.unexpressed.iter() {
        if row.kind == DeclKind::Macro.byte() && row.ns == NONE {
            let text = v.name(row.name);
            if fn_names.get(&text).is_some() || request.macro_fns.iter().any(|f| f.name == text) {
                continue;
            }
        }
        r.findings.push(Finding {
            kind: DeclKind::from_byte(row.kind).expect("checked by open"),
            name: row.name,
            ns: row.ns,
            cause: Unexpressed::from_byte(row.cause).expect("checked by open"),
            header: row.header,
            at: row.at,
            len: row.len,
            part: NONE,
        });
    }
    let dropped = |s: Status| match s {
        Status::Keep => None,
        Status::Drop(cause) => Some(Fail { cause, part: NONE }),
    };
    for (k, row) in v.records.iter().enumerate() {
        if let Some(f) = dropped(r.rec[k]) {
            // An anonymous record by the typedef that names it, when one does.
            let name = r.rec_name[k];
            r.header_finding(DeclKind::Record, name, row.ns, row.header, row.at, row.len, f);
        }
    }
    for (k, row) in v.enums.iter().enumerate() {
        if let Some(f) = dropped(r.enums[k]) {
            r.header_finding(DeclKind::Enum, row.name, row.ns, row.header, row.at, row.len, f);
        }
    }
    for (k, row) in v.typedefs.iter().enumerate() {
        // A typedef repeating its record's or enum's name is that type: its finding is theirs.
        if r.alias[k] {
            continue;
        }
        if let Some(f) = dropped(r.typedefs[k]) {
            r.header_finding(DeclKind::Typedef, row.name, row.ns, row.header, row.at, row.len, f);
        }
    }
    for (k, row) in v.fns.iter().enumerate() {
        if let Some(f) = dropped(r.fns[k]) {
            r.header_finding(DeclKind::Function, row.name, row.ns, row.header, row.at, row.len, f);
        }
    }
    for (k, row) in v.vars.iter().enumerate() {
        if let Some(f) = dropped(r.vars[k]) {
            r.header_finding(DeclKind::Variable, row.name, row.ns, row.header, row.at, row.len, f);
        }
    }
    for (k, row) in v.macros.iter().enumerate() {
        if let Some(f) = dropped(r.macros[k]) {
            r.header_finding(DeclKind::Macro, row.name, NONE, row.header, row.at, row.len, f);
        }
    }

    // The module.
    let mut out = String::new_in(alloc);
    out.push_str("pub mod ");
    out.push_str(request.module);
    out.push_str(" {\n#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]\n");
    scalar_asserts(&r, &mut out);
    // Each kept declaration rendered at the fixed point, so none fails here; one that does is a
    // bug of this module and stops the rendering naming the declaration (rule 3), never an item
    // quietly absent. Each renders into scratch first, so a failure leaves no half item.
    for k in 0..v.records.len() {
        if r.rec[k] == Status::Keep {
            let id = narrow(k).ok_or(RenderError::TooLarge)?;
            r.render_record(id, &mut out, true).map_err(|f| unsettled(DeclKind::Record, id, f))?;
        }
    }
    for k in 0..v.enums.len() {
        if r.enums[k] == Status::Keep {
            let id = narrow(k).ok_or(RenderError::TooLarge)?;
            r.render_enum(id, &mut out).map_err(|f| unsettled(DeclKind::Enum, id, f))?;
        }
    }
    for k in 0..v.typedefs.len() {
        if r.typedefs[k] == Status::Keep && !r.alias[k] {
            let id = narrow(k).ok_or(RenderError::TooLarge)?;
            r.render_typedef(id, &mut out).map_err(|f| unsettled(DeclKind::Typedef, id, f))?;
        }
    }
    // Per convention, the library's block (with `link`) and the unit's (without: its symbols are
    // the import's own).
    for conv in Conv::ALL {
        let mut block = String::new_in(alloc);
        let mut local = String::new_in(alloc);
        for k in 0..v.fns.len() {
            let f = v.fns[k];
            if r.fns[k] == Status::Keep && Conv::from_byte(v.types[at(f.ty)].extra) == Some(*conv) {
                let id = narrow(k).ok_or(RenderError::TooLarge)?;
                let into = if fn_is_local(&f) { &mut local } else { &mut block };
                r.render_fn(k, into).map_err(|e| unsettled(DeclKind::Function, id, e))?;
            }
        }
        if *conv == Conv::C {
            for k in 0..v.vars.len() {
                if r.vars[k] == Status::Keep {
                    let id = narrow(k).ok_or(RenderError::TooLarge)?;
                    let into = if var_is_local(&v.vars[k]) { &mut local } else { &mut block };
                    r.render_var(k, into).map_err(|e| unsettled(DeclKind::Variable, id, e))?;
                }
            }
            local.push_str(wrappers.as_str());
        }
        for (text, link) in [(&block, request.link), (&local, None)] {
            if text.as_str().is_empty() {
                continue;
            }
            // A function of a convention with no Rust spelling dropped at the fixed point, so a
            // block that is not empty has one.
            let abi = abi_name(*conv).map_err(|e| RenderError::Unsettled { kind: DeclKind::Function, row: NONE, cause: e.cause })?;
            if let Some(link) = link {
                format_into!(&mut out, "#[link(name = \"{link}\")]\n");
            }
            format_into!(&mut out, "unsafe extern \"{abi}\" {{\n");
            out.push_str(text.as_str());
            out.push_str("}\n");
        }
    }
    for (k, m) in v.macros.iter().enumerate() {
        if r.macros[k] == Status::Keep {
            let id = narrow(k).ok_or(RenderError::TooLarge)?;
            r.render_macro(m, &mut out).map_err(|e| unsettled(DeclKind::Macro, id, e))?;
        }
    }
    out.push_str("}\n");
    Ok(Rendered { rust: out, findings: r.findings })
}

fn unsettled(kind: DeclKind, row: u32, f: Fail) -> RenderError {
    RenderError::Unsettled { kind, row, cause: f.cause }
}
