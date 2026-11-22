//! The crate's promises, each with its oracle:
//!
//! - the record is a function of the model alone: declarations made in any order, types made
//!   once or at every use, write equal bytes, which open and round-trip (planted: functions in
//!   the builder's order, types not shared);
//! - the rendering of each construct (golden), and of a model shaped like a real C API (opaque
//!   handles, a callback with a `void*` state, an options struct, an error out-parameter);
//! - every declaration not rendered is a finding with its cause, C++ parts included;
//! - the grammar normalizes, and names the argument and range of each error;
//! - the request key changes with what changes the record and not with what does not;
//! - the read list is sorted, deduplicated, holds the probes that found nothing, refuses a file
//!   that changed, and says which read changed;
//! - an import over the model front end and a map of files;
//! - the differential compare names the first difference (planted: offsets, types or macro
//!   values not compared).

use core::fmt;
use std::{format, string::String as StdString, vec::Vec as StdVec};

use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc};
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

use crate::canon::{CanonPlant, encode};
use crate::differ::{Column, Difference, DifferPlant, Part, first_difference};
use crate::front::{FrontFault, ModelFrontEnd, ModelInclude, abi_row};
use crate::grammar::{Arg, Expected, GrammarError, GrammarFault, Request, Tok, TokKind, parse};
use crate::import::{Files, ImportError, ImportOpts, import};
use crate::key::{
    CAbi, CDialect, CTarget, Endian, ImportKey, KeyInputs, ReadsBuilder, ReadsError, ReadsView, RenderKey, RequestKey,
    Scalar, UnitKey,
};
use crate::model::{
    CType, Conv, DeclKind, DeclsBuilder, DeclsOpts, EnumConst, Field, Bitfield, FnAttrs, Function, IntKind, Linkage,
    MacroValue, MethodKind, ModelError, Name, NsId, Origin, Quals, RecordBody, RecordKind, Relocation, TypeId,
    Unexpressed, VarAttrs, VarInit, Variable, content_digest,
};
use crate::place::{Place, PlaceError, RelPath, Root};
use crate::render::{MacroFnFault, RenderError, render};
use crate::view::{DeclsView, Table};

/// A test text's offset as the `u32` the grammar takes.
fn offset(n: usize) -> u32 {
    u32::try_from(n).expect("a test text is short")
}

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const fn sc(size: u8, align: u8) -> Scalar {
    Scalar { size, align }
}

/// aarch64-apple-darwin's C.
const ABI: CAbi = CAbi {
    endian: Endian::Little,
    char_signed: true,
    pointer: sc(8, 8),
    bool_: sc(1, 1),
    short: sc(2, 2),
    int: sc(4, 4),
    long: sc(8, 8),
    long_long: sc(8, 8),
    int128: Some(sc(16, 16)),
    float: sc(4, 4),
    double: sc(8, 8),
    long_double: sc(8, 8),
    max_align: 8,
};

const TARGET: CTarget<'static> = CTarget { triple: "aarch64-apple-darwin", abi: ABI };

fn builder() -> DeclsBuilder<'static> {
    DeclsBuilder::new(DeclsOpts { abi: abi_row(&ABI), alloc: heap() })
}

/// The tokens of an argument list, as the expansion side would give them: identifiers, strings
/// (no escapes), integers and one-character punctuation, each with its byte range.
fn toks(text: &'static str) -> StdVec<Tok<'static>> {
    let bytes = text.as_bytes();
    let mut out = StdVec::new();
    let mut k = 0;
    while k < bytes.len() {
        let start = k;
        let b = bytes[k];
        let kind = if b.is_ascii_whitespace() {
            k += 1;
            continue;
        } else if b.is_ascii_alphabetic() || b == b'_' {
            while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                k += 1;
            }
            TokKind::Ident(&text[start..k])
        } else if b.is_ascii_digit() {
            while k < bytes.len() && bytes[k].is_ascii_alphanumeric() {
                k += 1;
            }
            TokKind::Int(&text[start..k])
        } else if b == b'"' {
            k += 1;
            while bytes[k] != b'"' {
                k += 1;
            }
            k += 1;
            TokKind::Str(&text[start + 1..k - 1])
        } else {
            k += 1;
            TokKind::Punct(char::from(b))
        };
        out.push(Tok { kind, at: offset(start), len: offset(k - start) });
    }
    out
}

fn request(text: &'static str) -> Request<'static> {
    let toks = toks(text);
    match parse(&toks, offset(text.len()), heap()) {
        Ok(request) => request,
        Err(e) => panic!("{text}: {e:?}"),
    }
}

fn parse_error(text: &'static str) -> GrammarError {
    let toks = toks(text);
    match parse(&toks, offset(text.len()), heap()) {
        Ok(_) => panic!("{text} parsed"),
        Err(e) => e,
    }
}

/// The C scalars, made once or at every use.
struct Types {
    fresh: bool,
    cache: StdVec<(u8, TypeId)>,
}

impl Types {
    fn new(fresh: bool) -> Types {
        Types { fresh, cache: StdVec::new() }
    }

    fn scalar(&mut self, b: &mut DeclsBuilder<'_>, which: u8) -> Result<TypeId, ModelError> {
        if !self.fresh
            && let Some((_, id)) = self.cache.iter().find(|(w, _)| *w == which)
        {
            return Ok(*id);
        }
        let int = |kind: IntKind, size: u32, signed: bool, quals: Quals| CType::Int { kind, size, align: size, signed, quals };
        let ty = match which {
            0 => CType::Void { quals: Quals::NONE },
            1 => int(IntKind::Int, 4, true, Quals::NONE),
            2 => int(IntKind::Char, 1, true, Quals::NONE),
            3 => int(IntKind::Char, 1, true, Quals::CONST),
            4 => int(IntKind::ULong, 8, false, Quals::NONE),
            5 => int(IntKind::UInt, 4, false, Quals::NONE),
            6 => int(IntKind::Long, 8, true, Quals::NONE),
            _ => unreachable!(),
        };
        let id = b.ty(ty)?;
        self.cache.push((which, id));
        Ok(id)
    }

    fn void(&mut self, b: &mut DeclsBuilder<'_>) -> Result<TypeId, ModelError> {
        self.scalar(b, 0)
    }
    fn int(&mut self, b: &mut DeclsBuilder<'_>) -> Result<TypeId, ModelError> {
        self.scalar(b, 1)
    }
    fn char(&mut self, b: &mut DeclsBuilder<'_>) -> Result<TypeId, ModelError> {
        self.scalar(b, 2)
    }
    fn const_char(&mut self, b: &mut DeclsBuilder<'_>) -> Result<TypeId, ModelError> {
        self.scalar(b, 3)
    }
    fn ulong(&mut self, b: &mut DeclsBuilder<'_>) -> Result<TypeId, ModelError> {
        self.scalar(b, 4)
    }
    fn uint(&mut self, b: &mut DeclsBuilder<'_>) -> Result<TypeId, ModelError> {
        self.scalar(b, 5)
    }
    fn long(&mut self, b: &mut DeclsBuilder<'_>) -> Result<TypeId, ModelError> {
        self.scalar(b, 6)
    }
}

fn ptr(b: &mut DeclsBuilder<'_>, to: TypeId) -> Result<TypeId, ModelError> {
    b.ty(CType::Pointer { pointee: to, quals: Quals::NONE })
}

fn func(b: &mut DeclsBuilder<'_>, result: TypeId, params: &[TypeId]) -> Result<TypeId, ModelError> {
    b.ty(CType::Function { result, params, variadic: false, prototyped: true, conv: Conv::C })
}

/// A free C function `name` of type `ty` with parameter names `params`.
fn function(b: &mut DeclsBuilder<'_>, name: &str, ty: TypeId, params: &[&str], at: u32) -> Result<(), ModelError> {
    function_of(b, name, ty, params, at, Linkage::External, false)
}

/// As [`function`], of `linkage`, with a body in the header or not.
fn function_of(
    b: &mut DeclsBuilder<'_>,
    name: &str,
    ty: TypeId,
    params: &[&str],
    at: u32,
    linkage: Linkage,
    body: bool,
) -> Result<(), ModelError> {
    let name = b.name(name)?;
    let mut names = StdVec::new();
    for p in params {
        names.push(Some(b.name(p)?));
    }
    let header = b.header(Root::Package, "api.h")?;
    b.function(Function {
        name,
        ns: NsId::GLOBAL,
        symbol: name,
        ty,
        param_names: &names,
        attrs: FnAttrs::NONE,
        owner: None,
        linkage,
        body,
        origin: Origin { header, at, len: 1 },
    })
}

/// A variable `name` of type `ty`, of `linkage` and `init`.
fn variable(b: &mut DeclsBuilder<'_>, name: &str, ty: TypeId, linkage: Linkage, init: VarInit) -> Result<crate::model::VarId, ModelError> {
    let name = b.name(name)?;
    let origin = origin(b, 0)?;
    b.variable(Variable { name, ns: NsId::GLOBAL, symbol: name, ty, attrs: VarAttrs::NONE, linkage, init, origin })
}

fn origin(b: &mut DeclsBuilder<'_>, at: u32) -> Result<Origin, ModelError> {
    Ok(Origin { header: b.header(Root::Package, "api.h")?, at, len: 1 })
}

fn field(name: Option<Name>, ty: TypeId, offset_bits: u64) -> Field {
    Field { name, ty, offset_bits, bitfield: None, at: 0, len: 0 }
}

fn body<'l>(size: u64, align: u32, pack: Option<u32>, fields: &'l [Field], units: &'l [(u32, u32)]) -> RecordBody<'l> {
    RecordBody { size, align, pack, fields, units, bases: &[], relocation: Relocation::TriviallyCopyable }
}

/// `struct NAME { FIELDS }` defined.
fn record(b: &mut DeclsBuilder<'_>, name: &str, body: RecordBody<'_>) -> Result<TypeId, ModelError> {
    let name = b.name(name)?;
    let origin = origin(b, 0)?;
    let rec = b.declare_record(RecordKind::Struct, Some(name), NsId::GLOBAL, origin)?;
    b.define_record(rec, body)?;
    b.ty(CType::Record { record: rec, quals: Quals::NONE })
}

fn encoded(b: &DeclsBuilder<'_>) -> AVec<'static, u8> {
    encode(b, heap(), NoPlant).expect("the model encodes")
}

/// The rendering of `b` for `args`, with no unit.
fn rendered(b: &DeclsBuilder<'_>, args: &'static str) -> (StdString, StdVec<crate::finding::Finding>) {
    rendered_in(b, args, None)
}

/// The rendering of `b` for `args`, bound to `unit`.
fn rendered_in(b: &DeclsBuilder<'_>, args: &'static str, unit: Option<UnitKey>) -> (StdString, StdVec<crate::finding::Finding>) {
    let record = encoded(b);
    let request = request(args);
    let out = render(&record, &request, unit, heap()).expect("the record renders");
    (out.rust.as_str().into(), out.findings.iter().copied().collect())
}

/// A unit of a fixed key (its first eight bytes `01 02 .. 08`), so its symbols are golden.
fn test_unit() -> UnitKey {
    let mut bytes = [0u8; 32];
    for (k, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::try_from(k + 1).expect("32 fits a byte");
    }
    UnitKey(ContentId(bytes))
}

/// The unit's symbol prefix for [`test_unit`].
const UNIT: &str = "__blaze_c_0102030405060708_";

fn assert_has(rust: &str, item: &str) {
    assert!(rust.contains(item), "the rendering lacks\n{item}\nin\n{rust}");
}

// ------------------------------------------------------------------- the record

/// The declarations of the determinism test, by number.
const DECLS: usize = 6;

/// Makes declaration `which` of the determinism test.
fn decl(b: &mut DeclsBuilder<'_>, t: &mut Types, which: usize) -> Result<(), ModelError> {
    match which {
        0..=3 => {
            let int = t.int(b)?;
            let ty = func(b, int, &[int])?;
            let name = ["alpha", "beta", "gamma", "delta"][which];
            function(b, name, ty, &["x"], 10 + offset(which))
        }
        4 => {
            let int = t.int(b)?;
            let (x, y) = (b.name("x")?, b.name("y")?);
            let pair = record(b, "pair", body(8, 4, None, &[field(Some(x), int, 0), field(Some(y), int, 32)], &[]))?;
            let p = ptr(b, pair)?;
            let void = t.void(b)?;
            let ty = func(b, void, &[p])?;
            function(b, "make_pair", ty, &["out"], 20)
        }
        _ => {
            let int = t.int(b)?;
            let name = b.name("LEVEL")?;
            let origin = origin(b, 30)?;
            b.constant(name, MacroValue::Int { bits: 3, ty: int }, origin)
        }
    }
}

fn build_in(order: &[usize], fresh: bool) -> DeclsBuilder<'static> {
    let mut b = builder();
    let mut t = Types::new(fresh);
    for which in order {
        decl(&mut b, &mut t, *which).expect("the model is well formed");
    }
    b
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CanonBroken {
    /// The bytes differ from the canonical build's.
    Differs { order: StdVec<usize>, fresh: bool },
}

impl fmt::Display for CanonBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CanonBroken::Differs { order, fresh } => write!(f, "order {order:?} (fresh types: {fresh}) writes other bytes"),
        }
    }
}

blaze_sim::plants! {
    impl CanonPlant breaks CanonBroken {
        None,
        FunctionsInBuilderOrder => CanonBroken::Differs { .. },
        TypesNotShared => CanonBroken::Differs { .. },
    }
}

fn one_order(tape: &Tape, plant: impl Planted<CanonPlant>) -> Result<(), Broken<CanonBroken>> {
    let canonical: StdVec<usize> = (0..DECLS).collect();
    let reference = encoded(&build_in(&canonical, false));
    let mut order = canonical.clone();
    tape.shuffle(&mut order, "order");
    let fresh = tape.coin("fresh");
    let bytes = encode(&build_in(&order, fresh), heap(), plant).map_err(|e| Broken::Harness(format!("{e:?}")))?;
    if bytes[..] != reference[..] {
        return Err(Broken::Oracle(CanonBroken::Differs { order, fresh }));
    }
    // It opens, and holds what was made.
    let view = DeclsView::open(&bytes).map_err(|e| Broken::Harness(format!("{e:?}")))?;
    if view.fns.len() != 5 || view.records.len() != 1 || view.macros.len() != 1 {
        return Err(Broken::Harness(format!("{} functions, {} records", view.fns.len(), view.records.len())));
    }
    Ok(())
}

#[test]
fn the_record_is_one_for_every_order_and_sharing() {
    check(Config::cases(128), |tape| one_order(tape, NoPlant));
}

#[test]
fn every_canon_plant_is_caught() {
    controls(Config::cases(128), |tape, plant: CanonPlant| one_order(tape, plant));
}

#[test]
fn the_record_round_trips_through_its_view() {
    let b = build_in(&[0, 1, 2, 3, 4, 5], false);
    let bytes = encoded(&b);
    let view = DeclsView::open(&bytes).expect("opens");
    let names: StdVec<&str> = view.fns.iter().map(|f| view.name(f.name)).collect();
    assert_eq!(names, ["alpha", "beta", "delta", "gamma", "make_pair"]);
    // `int(int)` is one type row for four functions.
    assert_eq!(view.fns[0].ty, view.fns[1].ty);
    let pair = view.records[0];
    assert_eq!(view.name(pair.name), "pair");
    let offsets: StdVec<u64> = view.fields_of(&pair).iter().map(|f| f.offset_bits).collect();
    assert_eq!(offsets, [0, 32]);
    assert_eq!(view.abi, abi_row(&ABI));
    // Encoding the same model twice writes the same bytes.
    assert_eq!(&encoded(&b)[..], &bytes[..]);
    // Truncated bytes are refused, not read.
    assert!(DeclsView::open(&bytes[..bytes.len() / 2]).is_err());
}

#[test]
fn a_name_that_is_not_an_identifier_is_refused() {
    let mut b = builder();
    assert!(matches!(b.name("not an identifier"), Err(ModelError::BadName { .. })));
    assert!(matches!(b.header(Root::Package, "/abs.h"), Err(ModelError::BadHeader { .. })));
}

// ------------------------------------------------------------------- the rendering

#[test]
fn a_struct_renders_with_its_layout_asserted() {
    let mut b = builder();
    let mut t = Types::new(false);
    let int = t.int(&mut b).unwrap();
    let (x, y) = (b.name("x").unwrap(), b.name("y").unwrap());
    record(&mut b, "point", body(8, 4, None, &[field(Some(x), int, 0), field(Some(y), int, 32)], &[])).unwrap();
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h";"#);
    assert!(rust.starts_with("pub mod m {\n#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]\n"));
    assert_has(
        &rust,
        "#[repr(C)]\n#[derive(Clone, Copy)]\npub struct point {\n    pub x: ::core::ffi::c_int,\n    pub y: ::core::ffi::c_int,\n}\nconst _: () = {\n    ::core::assert!(::core::mem::size_of::<point>() == 8);\n    ::core::assert!(::core::mem::align_of::<point>() == 4);\n    ::core::assert!(::core::mem::offset_of!(point, x) == 0);\n    ::core::assert!(::core::mem::offset_of!(point, y) == 4);\n};\n",
    );
    assert_has(&rust, "    ::core::assert!(::core::mem::size_of::<::core::ffi::c_int>() == 4);\n");
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn packed_aligned_and_flexible_records() {
    let mut b = builder();
    let mut t = Types::new(false);
    let (int, ch, uint) = (t.int(&mut b).unwrap(), t.char(&mut b).unwrap(), t.uint(&mut b).unwrap());
    let (c, i, x, len, data) = (b.name("c").unwrap(), b.name("i").unwrap(), b.name("x").unwrap(), b.name("len").unwrap(), b.name("data").unwrap());
    record(&mut b, "p", body(5, 1, Some(1), &[field(Some(c), ch, 0), field(Some(i), int, 8)], &[])).unwrap();
    record(&mut b, "a", body(16, 16, None, &[field(Some(x), int, 0)], &[])).unwrap();
    let fam = b.ty(CType::Array { elem: ch, len: None, quals: Quals::NONE }).unwrap();
    record(&mut b, "buf", body(4, 4, None, &[field(Some(len), uint, 0), field(Some(data), fam, 32)], &[])).unwrap();
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h";"#);
    assert_has(
        &rust,
        "#[repr(C, packed(1))]\n#[derive(Clone, Copy)]\npub struct p {\n    pub c: ::core::ffi::c_char,\n    pub i: ::core::ffi::c_int,\n}\nconst _: () = {\n    ::core::assert!(::core::mem::size_of::<p>() == 5);\n    ::core::assert!(::core::mem::align_of::<p>() == 1);\n    ::core::assert!(::core::mem::offset_of!(p, c) == 0);\n    ::core::assert!(::core::mem::offset_of!(p, i) == 1);\n};\n",
    );
    assert_has(&rust, "#[repr(C, align(16))]\n#[derive(Clone, Copy)]\npub struct a {\n    pub x: ::core::ffi::c_int,\n}\n");
    assert_has(&rust, "pub struct buf {\n    pub len: ::core::ffi::c_uint,\n    pub data: [::core::ffi::c_char; 0],\n}\n");
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn bitfields_render_as_a_unit_and_accessors() {
    let mut b = builder();
    let mut t = Types::new(false);
    let (int, uint) = (t.int(&mut b).unwrap(), t.uint(&mut b).unwrap());
    let (a, s, c) = (b.name("a").unwrap(), b.name("s").unwrap(), b.name("c").unwrap());
    let bits = |name, ty, offset_bits, width| Field { name: Some(name), ty, offset_bits, bitfield: Some(Bitfield { width, unit: 0 }), at: 0, len: 0 };
    let fields = [bits(a, uint, 0, 3), bits(s, int, 3, 5), bits(c, uint, 8, 1)];
    record(&mut b, "flags", body(4, 4, None, &fields, &[(0, 4)])).unwrap();
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h";"#);
    assert_has(&rust, "pub struct flags {\n    pub __bitfield_0: ::core::primitive::u32,\n}\n");
    assert_has(
        &rust,
        "    pub fn s(&self) -> ::core::ffi::c_int {\n        ((((self.__bitfield_0 >> 3) << 27) as ::core::primitive::i32) >> 27) as ::core::ffi::c_int\n    }\n    pub fn set_s(&mut self, value: ::core::ffi::c_int) {\n        let v = (value as ::core::primitive::u32) & 0x1f;\n        self.__bitfield_0 = (self.__bitfield_0 & !(0x1f << 3)) | (v << 3);\n    }\n",
    );
    assert_has(&rust, "    pub fn a(&self) -> ::core::ffi::c_uint {\n        ((self.__bitfield_0 >> 0) & 0x7) as ::core::ffi::c_uint\n    }\n");
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn an_enum_renders_as_a_newtype_with_constants() {
    let mut b = builder();
    let mut t = Types::new(false);
    let uint = t.uint(&mut b).unwrap();
    let name = b.name("level").unwrap();
    let origin = origin(&mut b, 0).unwrap();
    let e = b.declare_enum(Some(name), NsId::GLOBAL, origin).unwrap();
    let (low, high) = (b.name("LOW").unwrap(), b.name("HIGH").unwrap());
    let consts = [EnumConst { name: low, value: 0, at: 0, len: 0 }, EnumConst { name: high, value: 2, at: 0, len: 0 }];
    b.define_enum(e, uint, &consts, true, false).unwrap();
    // An anonymous enum: its constants are the module's.
    let anon = b.declare_enum(None, NsId::GLOBAL, origin).unwrap();
    let minus = b.name("MINUS").unwrap();
    let int = t.int(&mut b).unwrap();
    b.define_enum(anon, int, &[EnumConst { name: minus, value: u64::MAX, at: 0, len: 0 }], true, false).unwrap();
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h";"#);
    assert_has(
        &rust,
        "#[repr(transparent)]\n#[derive(Clone, Copy, PartialEq, Eq, Hash)]\npub struct level(pub ::core::ffi::c_uint);\nimpl level {\n    pub const LOW: level = level(0);\n    pub const HIGH: level = level(2);\n}\nconst _: () = ::core::assert!(::core::mem::size_of::<level>() == 4);\n",
    );
    assert_has(&rust, "pub const MINUS: ::core::ffi::c_int = -1;\n");
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn constants_keywords_and_function_pointers() {
    let mut b = builder();
    let mut t = Types::new(false);
    let (int, ch, void) = (t.int(&mut b).unwrap(), t.char(&mut b).unwrap(), t.void(&mut b).unwrap());
    let o = origin(&mut b, 0).unwrap();
    let n = b.name("MAJOR").unwrap();
    b.constant(n, MacroValue::Int { bits: 10, ty: int }, o).unwrap();
    let n = b.name("NAME").unwrap();
    b.constant(n, MacroValue::Str { bytes: b"rock\"db\n", ty: ch }, o).unwrap();
    let n = b.name("NUL").unwrap();
    b.constant(n, MacroValue::Str { bytes: b"a\0b", ty: ch }, o).unwrap();
    // A field named by a Rust keyword, and a callback field.
    let (ty_name, cb_name) = (b.name("type").unwrap(), b.name("cb").unwrap());
    let cb = func(&mut b, void, &[int]).unwrap();
    let pcb = ptr(&mut b, cb).unwrap();
    record(&mut b, "s", body(16, 8, None, &[field(Some(ty_name), int, 0), field(Some(cb_name), pcb, 64)], &[])).unwrap();
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h";"#);
    assert_has(&rust, "pub const MAJOR: ::core::ffi::c_int = 10;\n");
    assert_has(&rust, "pub const NAME: &::core::ffi::CStr = c\"rock\\\"db\\x0a\";\n");
    assert_has(&rust, "    pub r#type: ::core::ffi::c_int,\n    pub cb: ::core::option::Option<unsafe extern \"C\" fn(::core::ffi::c_int)>,\n");
    assert_has(&rust, "::core::mem::offset_of!(s, r#type) == 0");
    // The string with a NUL in it is not a `CStr`: a finding, not an item.
    assert!(!rust.contains("NUL"));
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!((findings[0].kind, findings[0].cause), (DeclKind::Macro, Unexpressed::StringWithNul));
}

/// A model shaped like rocksdb's C API: opaque handles, a callback with a `void*` state, an
/// options struct, an error out-parameter, constants.
fn rocksdb(b: &mut DeclsBuilder<'_>) -> Result<(), ModelError> {
    let mut t = Types::new(false);
    let (void, int, ch, cch, ulong) = (t.void(b)?, t.int(b)?, t.char(b)?, t.const_char(b)?, t.ulong(b)?);
    let header = b.header(Root::Package, "rocksdb/c.h")?;
    let o = Origin { header, at: 0, len: 1 };
    let pvoid = ptr(b, void)?;
    let pcch = ptr(b, cch)?;
    let pch = ptr(b, ch)?;
    let errptr = ptr(b, pch)?;
    let handle = |b: &mut DeclsBuilder<'_>, name: &str| -> Result<(TypeId, TypeId), ModelError> {
        let n = b.name(name)?;
        let rec = b.declare_record(RecordKind::Struct, Some(n), NsId::GLOBAL, o)?;
        let rty = b.ty(CType::Record { record: rec, quals: Quals::NONE })?;
        let td = b.typedef(n, NsId::GLOBAL, rty, o)?;
        let mutable = b.ty(CType::Typedef { decl: td, quals: Quals::NONE })?;
        let constant = b.ty(CType::Typedef { decl: td, quals: Quals::CONST })?;
        Ok((ptr(b, mutable)?, ptr(b, constant)?))
    };
    let (db, _) = handle(b, "rocksdb_t")?;
    let (opts, const_opts) = handle(b, "rocksdb_options_t")?;
    // typedef void (*rocksdb_logger_cb)(void* state, int level, const char* msg);
    let cb_fn = func(b, void, &[pvoid, int, pcch])?;
    let cb_ptr = ptr(b, cb_fn)?;
    let n = b.name("rocksdb_logger_cb")?;
    let cb_td = b.typedef(n, NsId::GLOBAL, cb_ptr, o)?;
    let cb = b.ty(CType::Typedef { decl: cb_td, quals: Quals::NONE })?;
    // typedef unsigned long size_t; typedef struct { int create_if_missing; size_t write_buffer_size; } ..;
    let n = b.name("size_t")?;
    let size_td = b.typedef(n, NsId::GLOBAL, ulong, o)?;
    let size_t = b.ty(CType::Typedef { decl: size_td, quals: Quals::NONE })?;
    let anon = b.declare_record(RecordKind::Struct, None, NsId::GLOBAL, o)?;
    let (f0, f1) = (b.name("create_if_missing")?, b.name("write_buffer_size")?);
    b.define_record(anon, body(16, 8, None, &[field(Some(f0), int, 0), field(Some(f1), size_t, 64)], &[]))?;
    let anon_ty = b.ty(CType::Record { record: anon, quals: Quals::NONE })?;
    let n = b.name("rocksdb_simple_options_t")?;
    let simple_td = b.typedef(n, NsId::GLOBAL, anon_ty, o)?;
    let simple = b.ty(CType::Typedef { decl: simple_td, quals: Quals::CONST })?;
    let psimple = ptr(b, simple)?;

    let ty = func(b, db, &[const_opts, pcch, errptr])?;
    function(b, "rocksdb_open", ty, &["options", "name", "errptr"], 100)?;
    let ty = func(b, void, &[db])?;
    function(b, "rocksdb_close", ty, &["db"], 200)?;
    let ty = func(b, void, &[opts, cb, pvoid])?;
    function(b, "rocksdb_options_set_logger", ty, &["opt", "cb", "state"], 300)?;
    let ty = func(b, void, &[psimple, errptr])?;
    function(b, "rocksdb_simple_open", ty, &["o", "errptr"], 400)?;
    let n = b.name("ROCKSDB_MAJOR")?;
    b.constant(n, MacroValue::Int { bits: 10, ty: int }, o)?;
    Ok(())
}

#[test]
fn a_real_c_api_renders() {
    let mut b = builder();
    rocksdb(&mut b).unwrap();
    let (rust, findings) = rendered(&b, r#"mod rocksdb; header "rocksdb/c.h"; link "rocksdb";"#);
    assert!(rust.starts_with("pub mod rocksdb {\n"));
    let opaque = |name: &str| {
        format!(
            "#[repr(C)]\npub struct {name} {{\n    __opaque: [::core::primitive::u8; 0],\n    __marker: ::core::marker::PhantomData<(*mut ::core::primitive::u8, ::core::marker::PhantomPinned)>,\n}}\n"
        )
    };
    assert_has(&rust, &opaque("rocksdb_t"));
    assert_has(&rust, &opaque("rocksdb_options_t"));
    assert_has(
        &rust,
        "pub type rocksdb_logger_cb = ::core::option::Option<unsafe extern \"C\" fn(*mut ::core::ffi::c_void, ::core::ffi::c_int, *const ::core::ffi::c_char)>;\n",
    );
    assert_has(&rust, "pub type size_t = ::core::ffi::c_ulong;\n");
    assert_has(
        &rust,
        "#[repr(C)]\n#[derive(Clone, Copy)]\npub struct rocksdb_simple_options_t {\n    pub create_if_missing: ::core::ffi::c_int,\n    pub write_buffer_size: size_t,\n}\nconst _: () = {\n    ::core::assert!(::core::mem::size_of::<rocksdb_simple_options_t>() == 16);\n    ::core::assert!(::core::mem::align_of::<rocksdb_simple_options_t>() == 8);\n    ::core::assert!(::core::mem::offset_of!(rocksdb_simple_options_t, create_if_missing) == 0);\n    ::core::assert!(::core::mem::offset_of!(rocksdb_simple_options_t, write_buffer_size) == 8);\n};\n",
    );
    assert_has(
        &rust,
        "#[link(name = \"rocksdb\")]\nunsafe extern \"C\" {\n    pub unsafe fn rocksdb_close(db: *mut rocksdb_t);\n    pub unsafe fn rocksdb_open(options: *const rocksdb_options_t, name: *const ::core::ffi::c_char, errptr: *mut *mut ::core::ffi::c_char) -> *mut rocksdb_t;\n    pub unsafe fn rocksdb_options_set_logger(opt: *mut rocksdb_options_t, cb: rocksdb_logger_cb, state: *mut ::core::ffi::c_void);\n    pub unsafe fn rocksdb_simple_open(o: *const rocksdb_simple_options_t, errptr: *mut *mut ::core::ffi::c_char);\n}\n",
    );
    assert_has(&rust, "pub const ROCKSDB_MAJOR: ::core::ffi::c_int = 10;\n");
    assert!(rust.ends_with("}\n"));
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn selection_drops_with_a_finding_and_keeps_what_is_used() {
    let mut b = builder();
    rocksdb(&mut b).unwrap();
    let (rust, findings) = rendered(&b, r#"mod rocksdb; header "rocksdb/c.h"; allow "rocksdb_open";"#);
    assert_has(&rust, "pub unsafe fn rocksdb_open(");
    // The types it uses are kept although the pattern does not name them.
    assert_has(&rust, "pub struct rocksdb_options_t {");
    assert!(!rust.contains("rocksdb_close"));
    assert!(!rust.contains("rocksdb_logger_cb"));
    assert!(findings.iter().all(|f| f.cause == Unexpressed::NotSelected), "{findings:?}");
    let view_bytes = encoded(&b);
    let view = DeclsView::open(&view_bytes).unwrap();
    let dropped: StdVec<&str> = findings.iter().filter_map(|f| f.name(&view)).collect();
    for name in ["rocksdb_close", "rocksdb_options_set_logger", "rocksdb_simple_open", "ROCKSDB_MAJOR", "rocksdb_logger_cb"] {
        assert!(dropped.contains(&name), "{name} has no finding: {dropped:?}");
    }
}

#[test]
fn cpp_parts_are_findings_not_skips() {
    let mut b = builder();
    let mut t = Types::new(false);
    let (int, void) = (t.int(&mut b).unwrap(), t.void(&mut b).unwrap());
    let o = origin(&mut b, 0).unwrap();
    let ty = func(&mut b, void, &[int]).unwrap();
    // A function in a namespace.
    let ns_name = b.name("rocks").unwrap();
    let ns = b.namespace(ns_name, NsId::GLOBAL).unwrap();
    let f = b.name("open").unwrap();
    b.function(Function { name: f, ns, symbol: f, ty, param_names: &[None], attrs: FnAttrs::NONE, owner: None, linkage: Linkage::External, body: false, origin: o }).unwrap();
    // An overload set of two: one name, two linker symbols.
    let (g, g1, g2) = (b.name("put").unwrap(), b.name("_Z3puti").unwrap(), b.name("_Z3putl").unwrap());
    for symbol in [g1, g2] {
        b.function(Function { name: g, ns: NsId::GLOBAL, symbol, ty, param_names: &[None], attrs: FnAttrs::NONE, owner: None, linkage: Linkage::External, body: false, origin: o }).unwrap();
    }
    // A method of a record that is not trivially relocatable.
    let r = b.name("db").unwrap();
    let rec = b.declare_record(RecordKind::Struct, Some(r), NsId::GLOBAL, o).unwrap();
    b.define_record(rec, RecordBody { size: 8, align: 8, pack: None, fields: &[], units: &[], bases: &[], relocation: Relocation::AddressOnly }).unwrap();
    let m = b.name("flush").unwrap();
    b.function(Function { name: m, ns: NsId::GLOBAL, symbol: m, ty, param_names: &[None], attrs: FnAttrs::NONE, owner: Some((rec, MethodKind::InstanceMut)), linkage: Linkage::External, body: false, origin: o }).unwrap();
    // A function that may throw.
    let h = b.name("may_throw").unwrap();
    b.function(Function { name: h, ns: NsId::GLOBAL, symbol: h, ty, param_names: &[None], attrs: FnAttrs::MAY_THROW, owner: None, linkage: Linkage::External, body: false, origin: o }).unwrap();
    // A construct the front end could not hold.
    let x = b.name("CHECK").unwrap();
    b.unexpressed(x, NsId::GLOBAL, DeclKind::Macro, Unexpressed::FunctionLikeMacro, o).unwrap();
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h";"#);
    let bytes = encoded(&b);
    let view = DeclsView::open(&bytes).unwrap();
    let cause_of = |name: &str| -> StdVec<Unexpressed> { findings.iter().filter(|f| f.name(&view) == Some(name)).map(|f| f.cause).collect() };
    assert_eq!(cause_of("open"), [Unexpressed::CppNamespace]);
    assert_eq!(cause_of("put"), [Unexpressed::CppOverloads, Unexpressed::CppOverloads]);
    assert_eq!(cause_of("flush"), [Unexpressed::CppMethod]);
    assert_eq!(cause_of("db"), [Unexpressed::CppRelocation]);
    assert_eq!(cause_of("may_throw"), [Unexpressed::CppMayThrow]);
    assert_eq!(cause_of("CHECK"), [Unexpressed::FunctionLikeMacro]);
    assert!(!rust.contains("extern"), "{rust}");
}

#[test]
fn a_layout_repr_c_cannot_make_is_bytes_and_a_finding() {
    let mut b = builder();
    let mut t = Types::new(false);
    let int = t.int(&mut b).unwrap();
    let x = b.name("x").unwrap();
    // An int at byte 2 with no packing: `repr(C)` cannot place it there.
    record(&mut b, "odd", body(8, 4, None, &[field(Some(x), int, 16)], &[])).unwrap();
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h";"#);
    assert_has(&rust, "#[repr(C, align(4))]\n#[derive(Clone, Copy)]\npub struct odd {\n    __bytes: [::core::primitive::u8; 8],\n}\n");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!((findings[0].kind, findings[0].cause, findings[0].whole()), (DeclKind::Record, Unexpressed::LayoutNotReproduced, true));
}

// ------------------------------------------------------------------- the header's own code

/// Findings as names and causes, compared with a list of `(name, cause)`.
#[derive(Debug)]
struct Causes(StdVec<(StdString, Unexpressed)>);

impl<const N: usize> PartialEq<[(&str, Unexpressed); N]> for Causes {
    fn eq(&self, want: &[(&str, Unexpressed); N]) -> bool {
        self.0.len() == N && self.0.iter().zip(want.iter()).all(|((n, c), (w, d))| n.as_str() == *w && c == d)
    }
}

/// Each finding's name and cause, sorted by name (stable: one name's causes stay in order).
fn named_causes(b: &DeclsBuilder<'_>, findings: &[crate::finding::Finding]) -> Causes {
    let bytes = encoded(b);
    let view = DeclsView::open(&bytes).unwrap();
    let mut out: StdVec<(StdString, Unexpressed)> = findings.iter().map(|f| (f.name(&view).unwrap_or("").into(), f.cause)).collect();
    out.sort_by(|x, y| x.0.cmp(&y.0));
    Causes(out)
}

fn render_error(b: &DeclsBuilder<'_>, args: &'static str, unit: Option<UnitKey>) -> RenderError {
    let record = encoded(b);
    let request = request(args);
    match render(&record, &request, unit, heap()) {
        Ok(out) => panic!("{args} rendered:\n{}", out.rust.as_str()),
        Err(e) => e,
    }
}

/// `struct PyObject { long ob_refcnt; }`, reduced from cpython object.h's.
fn py_object(b: &mut DeclsBuilder<'_>, t: &mut Types) -> Result<TypeId, ModelError> {
    let long = t.long(b)?;
    let refcnt = b.name("ob_refcnt")?;
    record(b, "PyObject", body(8, 8, None, &[field(Some(refcnt), long, 0)], &[]))
}

/// `static inline Py_ssize_t Py_REFCNT(PyObject *ob) { return ob->ob_refcnt; }` (cpython
/// object.h:316), `Py_ssize_t` as `long`.
fn py_refcnt(b: &mut DeclsBuilder<'_>) -> Result<(), ModelError> {
    let mut t = Types::new(false);
    let object = py_object(b, &mut t)?;
    let p = ptr(b, object)?;
    let long = t.long(b)?;
    let ty = func(b, long, &[p])?;
    function_of(b, "Py_REFCNT", ty, &["ob"], 316, Linkage::Inline, true)
}

#[test]
fn a_header_body_renders_as_its_thunk_in_the_unit() {
    let mut b = builder();
    py_refcnt(&mut b).unwrap();
    // The library's block would carry `link`; the thunk's is the import's own, without it.
    let (rust, findings) = rendered_in(&b, r#"mod m; header "api.h"; link "python3";"#, Some(test_unit()));
    assert_has(
        &rust,
        "unsafe extern \"C\" {\n    #[link_name = \"__blaze_c_0102030405060708_Py_REFCNT\"]\n    pub unsafe fn Py_REFCNT(ob: *mut PyObject) -> ::core::ffi::c_long;\n}\n",
    );
    assert!(!rust.contains("#[link(name"), "{rust}");
    assert!(findings.is_empty(), "{findings:?}");
    // No unit, or bodies not asked for: a finding, never a call of a symbol no library has.
    for (args, unit) in [(r#"mod m; header "api.h";"#, None), (r#"mod m; header "api.h"; inline_bodies off;"#, Some(test_unit()))] {
        let (rust, findings) = rendered_in(&b, args, unit);
        assert!(!rust.contains("Py_REFCNT"), "{args}:\n{rust}");
        assert_eq!(named_causes(&b, &findings), [("Py_REFCNT", Unexpressed::InlineFunction)], "{args}");
    }
}

#[test]
fn a_header_function_without_a_body_or_with_dots_is_a_finding() {
    let mut b = builder();
    let mut t = Types::new(false);
    let (int, cch) = (t.int(&mut b).unwrap(), t.const_char(&mut b).unwrap());
    let pcch = ptr(&mut b, cch).unwrap();
    // `static int f(void);` with no definition in the headers: no code to compile.
    let ty = func(&mut b, int, &[]).unwrap();
    function_of(&mut b, "no_body", ty, &[], 10, Linkage::Internal, false).unwrap();
    // `static inline int log_it(const char *fmt, ...) { .. }`: a thunk cannot pass `...` on.
    let ty = b.ty(CType::Function { result: int, params: &[pcch], variadic: true, prototyped: true, conv: Conv::C }).unwrap();
    function_of(&mut b, "log_it", ty, &["fmt"], 20, Linkage::Inline, true).unwrap();
    let (rust, findings) = rendered_in(&b, r#"mod m; header "api.h";"#, Some(test_unit()));
    assert!(!rust.contains("extern"), "{rust}");
    assert_eq!(
        named_causes(&b, &findings),
        [("log_it",Unexpressed::InlineFunction), ("no_body",Unexpressed::InlineFunction)]
    );
}

/// numpy's `static inline int PyArray_NDIM(const PyArrayObject *arr)`: declared ahead
/// (ndarraytypes.h:1344) and defined later (ndarraytypes.h:1594); `const` dropped here.
fn py_array_ndim(b: &mut DeclsBuilder<'_>, forward_first: bool, linkage: Linkage, body_twice: bool) -> Result<(), ModelError> {
    let mut t = Types::new(false);
    let int = t.int(b)?;
    let nd = b.name("nd")?;
    let arr = record(b, "PyArrayObject", body(4, 4, None, &[field(Some(nd), int, 0)], &[]))?;
    let p = ptr(b, arr)?;
    let ty = func(b, int, &[p])?;
    let forward = |b: &mut DeclsBuilder<'_>| function_of(b, "PyArray_NDIM", ty, &["arr"], 1344, Linkage::Inline, body_twice);
    let define = |b: &mut DeclsBuilder<'_>| function_of(b, "PyArray_NDIM", ty, &["arr"], 1594, linkage, true);
    match forward_first {
        true => {
            forward(b)?;
            define(b)
        }
        false => {
            define(b)?;
            forward(b)
        }
    }
}

#[test]
fn a_forward_declaration_and_its_body_are_one_row() {
    let build = |forward_first: bool, linkage: Linkage, body_twice: bool| {
        let mut b = builder();
        py_array_ndim(&mut b, forward_first, linkage, body_twice).unwrap();
        b
    };
    let (a, z) = (build(true, Linkage::Inline, false), build(false, Linkage::Inline, false));
    let bytes = encoded(&a);
    assert_eq!(bytes.as_slice(), encoded(&z).as_slice(), "the merge depends on the builder's order");
    let view = DeclsView::open(&bytes).unwrap();
    assert_eq!(view.fns.len(), 1);
    let row = view.fns[0];
    assert_eq!((row.body, row.linkage, row.at), (1, Linkage::Inline.byte(), 1594), "the definition is the row kept");
    let (rust, findings) = rendered_in(&a, r#"mod m; header "api.h";"#, Some(test_unit()));
    assert_has(
        &rust,
        "    #[link_name = \"__blaze_c_0102030405060708_PyArray_NDIM\"]\n    pub unsafe fn PyArray_NDIM(arr: *mut PyArrayObject) -> ::core::ffi::c_int;\n",
    );
    assert_eq!(rust.matches("fn PyArray_NDIM").count(), 1, "{rust}");
    assert!(findings.is_empty(), "{findings:?}");
    // Two bodies, or a declaration of another linkage: two declarations, refused by name.
    for (linkage, body_twice) in [(Linkage::Inline, true), (Linkage::External, false)] {
        let b = build(true, linkage, body_twice);
        let e = encode(&b, heap(), NoPlant).map(|_| ()).unwrap_err();
        assert!(matches!(e, ModelError::Redeclared { .. }), "{linkage:?} {body_twice}: {e:?}");
    }
}

/// numpy's API table and two of its slots (__multiarray_api.h): `static void **PyArray_API =
/// NULL;` (:644), `#define PyArray_GetNDArrayCVersion (*(unsigned int (*)(void)) PyArray_API[0])`
/// (:650) and `#define PyArray_Type (*(PyTypeObject *)PyArray_API[2])` (:652).
fn numpy_api(b: &mut DeclsBuilder<'_>) -> Result<(), ModelError> {
    let mut t = Types::new(false);
    let (void, uint, long) = (t.void(b)?, t.uint(b)?, t.long(b)?);
    let pvoid = ptr(b, void)?;
    let table_ty = ptr(b, pvoid)?;
    let table = variable(b, "PyArray_API", table_ty, Linkage::Internal, VarInit::Zero)?;
    let size = b.name("tp_basicsize")?;
    let type_object = record(b, "PyTypeObject", body(8, 8, None, &[field(Some(size), long, 0)], &[]))?;
    let ptype = ptr(b, type_object)?;
    let version = func(b, uint, &[])?;
    let pversion = ptr(b, version)?;
    let o = origin(b, 650)?;
    let n = b.name("PyArray_GetNDArrayCVersion")?;
    b.constant(n, MacroValue::Slot { table, index: 0, ty: pversion, deref: true }, o)?;
    let o = origin(b, 652)?;
    let n = b.name("PyArray_Type")?;
    b.constant(n, MacroValue::Slot { table, index: 2, ty: ptype, deref: true }, o)
}

#[test]
fn a_header_variable_is_the_units_and_its_slots_read_it() {
    let mut b = builder();
    numpy_api(&mut b).unwrap();
    let (rust, findings) = rendered_in(&b, r#"mod m; header "api.h";"#, Some(test_unit()));
    // One instance per import: the unit's, reached by its address; no `static mut`.
    assert_has(
        &rust,
        "unsafe extern \"C\" {\n    #[link_name = \"__blaze_c_0102030405060708_PyArray_API\"]\n    pub safe fn PyArray_API() -> *mut *mut *mut ::core::ffi::c_void;\n}\n",
    );
    assert!(!rust.contains("static mut"), "{rust}");
    // The data slot: the cast pointer, after a check that the table was filled.
    assert_has(
        &rust,
        r##"/// `PyArray_Type`: entry 2 of the table `PyArray_API`, as the header's macro reads it. The macro reads through the cast pointer (`*(T *)table[i]`): this is that pointer.
///
/// # Safety
///
/// `PyArray_API` is null until the library's import call has filled it (numpy's
/// `import_array()` is one), and only the header says entry 2 has this type.
#[inline]
pub unsafe fn PyArray_Type() -> *mut PyTypeObject {
    let table = unsafe { (PyArray_API()).read() };
    ::core::assert!(!table.is_null(), "`PyArray_API` is null: the library's import call has not run");
    (unsafe { table.add(2).read() }) as *mut PyTypeObject
}
"##,
    );
    // The function slot is callable: a function of the entry's signature.
    assert_has(&rust, "#[inline]\npub unsafe fn PyArray_GetNDArrayCVersion() -> ::core::ffi::c_uint {\n    let table = unsafe { (PyArray_API()).read() };\n");
    assert_has(&rust, "unsafe { ::core::mem::transmute(table.add(0).read()) };\n");
    assert_has(
        &rust,
        "    let ::core::option::Option::Some(f) = entry else {\n        ::core::panic!(\"`PyArray_API[0]` is null: the library's table has no entry there\")\n    };\n    unsafe { f() }\n}\n",
    );
    assert!(findings.is_empty(), "{findings:?}");
    // Without a unit the variable has no instance: it and its slots are findings.
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h";"#);
    assert!(!rust.contains("PyArray_API"), "{rust}");
    assert_eq!(
        named_causes(&b, &findings),
        [
            ("PyArray_API",Unexpressed::HeaderVariable),
            ("PyArray_GetNDArrayCVersion",Unexpressed::DependsOnDropped),
            ("PyArray_Type",Unexpressed::DependsOnDropped),
        ]
    );
}

#[test]
fn a_variables_linkage_and_initializer_are_checked() {
    let mut b = builder();
    let mut t = Types::new(false);
    let int = t.int(&mut b).unwrap();
    // `static int n;` with no initializer is a tentative definition C makes zero: the front end
    // says `Zero`; `Absent` on a header-local variable, or an `inline` variable, is refused.
    let e = variable(&mut b, "n", int, Linkage::Internal, VarInit::Absent).unwrap_err();
    assert!(matches!(e, ModelError::BadLinkage { .. }), "{e:?}");
    let e = variable(&mut b, "m", int, Linkage::Inline, VarInit::Zero).unwrap_err();
    assert!(matches!(e, ModelError::BadLinkage { .. }), "{e:?}");
    variable(&mut b, "counter", int, Linkage::Internal, VarInit::Int(7)).unwrap();
    let bytes = encoded(&b);
    let view = DeclsView::open(&bytes).unwrap();
    let row = view.vars[0];
    assert_eq!((row.linkage, row.init, row.init_bits), (Linkage::Internal.byte(), crate::model::init::INT, 7));
    let (rust, _) = rendered_in(&b, r#"mod m; header "api.h";"#, Some(test_unit()));
    assert_has(&rust, "    pub safe fn counter() -> *mut ::core::ffi::c_int;\n");
}

#[test]
fn an_address_macro_is_a_function_returning_the_pointer() {
    let mut b = builder();
    let mut t = Types::new(false);
    let object = py_object(&mut b, &mut t).unwrap();
    // `PyAPI_DATA(PyObject) _Py_NoneStruct;` and `#define Py_None (&_Py_NoneStruct)` (cpython
    // object.h:1106).
    let none = variable(&mut b, "_Py_NoneStruct", object, Linkage::External, VarInit::Absent).unwrap();
    let n = b.name("Py_None").unwrap();
    let o = origin(&mut b, 1106).unwrap();
    b.constant(n, MacroValue::AddressOf { variable: none }, o).unwrap();
    // Selecting the macro keeps the variable it names, and the type it reaches.
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h"; allow "Py_None";"#);
    assert_has(&rust, "    pub static mut _Py_NoneStruct: PyObject;\n");
    assert_has(&rust, "/// `Py_None`: the address of `_Py_NoneStruct`.\n#[inline]\npub fn Py_None() -> *mut PyObject {\n    &raw mut _Py_NoneStruct\n}\n");
    assert_has(&rust, "pub struct PyObject {");
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn macro_causes_and_a_function_of_the_macros_name() {
    let mut b = builder();
    let mut t = Types::new(false);
    let o = origin(&mut b, 0).unwrap();
    let causes = [
        // `#define PyObject_HEAD PyObject ob_base;` (cpython object.h:60): a declaration.
        ("PyObject_HEAD", Unexpressed::MacroIsFragment),
        // `#define Py_RETURN_NONE return Py_None` (object.h:1118): a statement.
        ("Py_RETURN_NONE", Unexpressed::MacroIsFragment),
        // `Py_TRASHCAN_BEGIN(op, dealloc)` (cpython/object.h:489): opens a block it does not close.
        ("Py_TRASHCAN_BEGIN", Unexpressed::MacroIsFragment),
        // `PyArray_IsScalar(obj, cls)` (numpy ndarrayobject.h:40): pastes `Py##cls##ArrType_Type`.
        ("PyArray_IsScalar", Unexpressed::MacroPastesOrAssigns),
        // `PyArray_HasArrayInterfaceType(op, type, context, out)` (ndarrayobject.h:28): assigns `(out)`.
        ("PyArray_HasArrayInterfaceType", Unexpressed::MacroPastesOrAssigns),
        // `#define Py_Is(x, y) ((x) == (y))` (object.h:235): could be a function, and is one.
        ("Py_Is", Unexpressed::FunctionLikeMacro),
    ];
    for (name, cause) in causes {
        let n = b.name(name).unwrap();
        b.unexpressed(n, NsId::GLOBAL, DeclKind::Macro, cause, o).unwrap();
    }
    // `PyAPI_FUNC(int) Py_Is(PyObject *x, PyObject *y);` (object.h:234): the function wins.
    let (void, int) = (t.void(&mut b).unwrap(), t.int(&mut b).unwrap());
    let pvoid = ptr(&mut b, void).unwrap();
    let ty = func(&mut b, int, &[pvoid, pvoid]).unwrap();
    function(&mut b, "Py_Is", ty, &["x", "y"], 234).unwrap();
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h";"#);
    assert_has(&rust, "    pub unsafe fn Py_Is(x: *mut ::core::ffi::c_void, y: *mut ::core::ffi::c_void) -> ::core::ffi::c_int;\n");
    assert_eq!(
        named_causes(&b, &findings),
        [
            ("PyArray_HasArrayInterfaceType",Unexpressed::MacroPastesOrAssigns),
            ("PyArray_IsScalar",Unexpressed::MacroPastesOrAssigns),
            ("PyObject_HEAD",Unexpressed::MacroIsFragment),
            ("Py_RETURN_NONE",Unexpressed::MacroIsFragment),
            ("Py_TRASHCAN_BEGIN",Unexpressed::MacroIsFragment),
        ]
    );
}

/// A function-like macro of numpy's (`#define PyArray_ISCONTIGUOUS(m) PyArray_CHKFLAGS((m),
/// NPY_ARRAY_C_CONTIGUOUS)`, ndarraytypes.h) with the record it takes, and the fragment
/// `Py_RETURN_NONE` (cpython object.h:1118).
fn macro_fn_model(b: &mut DeclsBuilder<'_>) -> Result<(), ModelError> {
    let mut t = Types::new(false);
    let int = t.int(b)?;
    let nd = b.name("nd")?;
    record(b, "PyArrayObject", body(4, 4, None, &[field(Some(nd), int, 0)], &[]))?;
    let o = origin(b, 0)?;
    let n = b.name("PyArray_ISCONTIGUOUS")?;
    b.unexpressed(n, NsId::GLOBAL, DeclKind::Macro, Unexpressed::FunctionLikeMacro, o)?;
    let n = b.name("Py_RETURN_NONE")?;
    b.unexpressed(n, NsId::GLOBAL, DeclKind::Macro, Unexpressed::MacroIsFragment, o)
}

#[test]
fn a_macro_fn_is_the_units_wrapper() {
    let mut b = builder();
    macro_fn_model(&mut b).unwrap();
    let args = r#"mod m; header "api.h"; link "npymath"; macro_fn PyArray_ISCONTIGUOUS(m: *mut PyArrayObject) -> c_int;"#;
    let (rust, findings) = rendered_in(&b, args, Some(test_unit()));
    assert_has(
        &rust,
        "unsafe extern \"C\" {\n    #[link_name = \"__blaze_c_0102030405060708_PyArray_ISCONTIGUOUS\"]\n    pub unsafe fn PyArray_ISCONTIGUOUS(m: *mut PyArrayObject) -> ::core::ffi::c_int;\n}\n",
    );
    assert!(!rust.contains("#[link(name"), "{rust}");
    assert_has(&rust, "pub struct PyArrayObject {");
    // The macro asked for is rendered, so it is no finding; the fragment still is.
    assert_eq!(named_causes(&b, &findings), [("Py_RETURN_NONE",Unexpressed::MacroIsFragment)]);
}

#[test]
fn each_macro_fn_refusal_names_the_ask() {
    let mut b = builder();
    macro_fn_model(&mut b).unwrap();
    let mut t = Types::new(false);
    let (void, int) = (t.void(&mut b).unwrap(), t.int(&mut b).unwrap());
    let pvoid = ptr(&mut b, void).unwrap();
    let ty = func(&mut b, int, &[pvoid, pvoid]).unwrap();
    function(&mut b, "Py_Is", ty, &["x", "y"], 234).unwrap();
    let o = origin(&mut b, 0).unwrap();
    let n = b.name("Py_Is").unwrap();
    b.unexpressed(n, NsId::GLOBAL, DeclKind::Macro, Unexpressed::FunctionLikeMacro, o).unwrap();
    let unit = Some(test_unit());
    let fault = |args: &'static str, unit: Option<UnitKey>| match render_error(&b, args, unit) {
        RenderError::MacroFn { index: 0, fault } => fault,
        other => panic!("{args}: {other:?}"),
    };
    let ok = r#"mod m; header "api.h"; macro_fn PyArray_ISCONTIGUOUS(m: *mut PyArrayObject) -> c_int;"#;
    assert_eq!(fault(ok, None), MacroFnFault::NoUnit);
    assert_eq!(fault(r#"mod m; header "api.h"; macro_fn Nope();"#, unit), MacroFnFault::NoSuchMacro);
    assert_eq!(
        fault(r#"mod m; header "api.h"; macro_fn Py_RETURN_NONE();"#, unit),
        MacroFnFault::NotAFunction(Unexpressed::MacroIsFragment)
    );
    assert_eq!(fault(r#"mod m; header "api.h"; macro_fn Py_Is(x: *mut c_void, y: *mut c_void) -> c_int;"#, unit), MacroFnFault::IsAFunction);
    assert_eq!(
        fault(r#"mod m; header "api.h"; macro_fn PyArray_ISCONTIGUOUS(m: *mut Missing) -> c_int;"#, unit),
        MacroFnFault::UnknownType { part: 0 }
    );
    assert_eq!(
        fault(r#"mod m; header "api.h"; macro_fn PyArray_ISCONTIGUOUS(m: *mut PyArrayObject) -> Missing;"#, unit),
        MacroFnFault::UnknownType { part: crate::NONE }
    );
    // A value of the module holds the name already.
    let mut c = builder();
    macro_fn_model(&mut c).unwrap();
    let int = Types::new(false).int(&mut c).unwrap();
    variable(&mut c, "PyArray_ISCONTIGUOUS", int, Linkage::External, VarInit::Absent).unwrap();
    match render_error(&c, ok, unit) {
        RenderError::MacroFn { index: 0, fault } => assert_eq!(fault, MacroFnFault::NameClash),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_macro_constant_of_a_typedef_or_an_enum_is_of_that_type() {
    let mut b = builder();
    let mut t = Types::new(false);
    let (long, uint) = (t.long(&mut b).unwrap(), t.uint(&mut b).unwrap());
    let o = origin(&mut b, 0).unwrap();
    // `typedef long Py_ssize_t;` and `#define PY_SSIZE_T_MAX ((Py_ssize_t)(((size_t)-1)>>1))`
    // (cpython pyport.h).
    let n = b.name("Py_ssize_t").unwrap();
    let td = b.typedef(n, NsId::GLOBAL, long, o).unwrap();
    let ssize = b.ty(CType::Typedef { decl: td, quals: Quals::NONE }).unwrap();
    let n = b.name("PY_SSIZE_T_MAX").unwrap();
    b.constant(n, MacroValue::Int { bits: u64::MAX >> 1, ty: ssize }, o).unwrap();
    // `enum NPY_TYPES { .., NPY_DOUBLE = 12, .. }` and `#define NPY_DEFAULT_TYPE NPY_DOUBLE`
    // (numpy ndarraytypes.h).
    let n = b.name("NPY_TYPES").unwrap();
    let e = b.declare_enum(Some(n), NsId::GLOBAL, o).unwrap();
    let double = b.name("NPY_DOUBLE").unwrap();
    b.define_enum(e, uint, &[EnumConst { name: double, value: 12, at: 0, len: 0 }], true, false).unwrap();
    let ety = b.ty(CType::Enum { decl: e, quals: Quals::NONE }).unwrap();
    let n = b.name("NPY_DEFAULT_TYPE").unwrap();
    b.constant(n, MacroValue::Int { bits: 12, ty: ety }, o).unwrap();
    let (rust, findings) = rendered(&b, r#"mod m; header "api.h";"#);
    assert_has(&rust, "pub const PY_SSIZE_T_MAX: Py_ssize_t = 9223372036854775807;\n");
    assert_has(&rust, "pub const NPY_DEFAULT_TYPE: NPY_TYPES = NPY_TYPES(12);\n");
    assert!(findings.is_empty(), "{findings:?}");
}

// ------------------------------------------------------------------- the grammar

#[test]
fn the_grammar_normalizes() {
    let r = request(
        r#"mod rocks; header "b.h"; header "a.h"; include "x"; include "y"; include "x"; define B = 2; define A; define C = -1; allow "z*"; allow "a"; allow "a"; block "a_internal"; link "rocksdb";"#,
    );
    assert_eq!(r.module, "rocks");
    let headers: StdVec<&str> = r.headers.iter().map(|h| h.as_str()).collect();
    assert_eq!(headers, ["b.h", "a.h"]);
    let include: StdVec<&str> = r.include.iter().map(|h| h.as_str()).collect();
    assert_eq!(include, ["x", "y"]);
    let defines: StdVec<(&str, Option<(bool, &str)>)> = r.defines.iter().map(|d| (d.name, d.value.map(|v| (v.negative, v.text)))).collect();
    assert_eq!(defines, [("A", None), ("B", Some((false, "2"))), ("C", Some((true, "1")))]);
    let allow: StdVec<(&str, bool)> = r.allow.iter().map(|p| (p.text, p.prefix)).collect();
    assert_eq!(allow, [("a", false), ("z", true)]);
    assert_eq!(r.link, Some("rocksdb"));
    assert!(r.selects("zeta") && r.selects("a") && !r.selects("a_internal") && !r.selects("b"));
}

#[test]
fn grammar_errors_name_the_argument_and_range() {
    let e = parse_error(r#"mod m; mod n; header "a.h";"#);
    assert_eq!((e.arg, e.fault, e.at, e.len), (Some(Arg::Mod), GrammarFault::Repeated, 7, 3));
    let e = parse_error(r#"mod m; header "/a.h";"#);
    assert_eq!((e.arg, e.at, e.len), (Some(Arg::Header), 14, 6));
    assert!(matches!(e.fault, GrammarFault::Path(PlaceError::Absolute)));
    let e = parse_error(r#"mod m; header "../a.h";"#);
    assert!(matches!(e.fault, GrammarFault::Path(PlaceError::Parent { .. })));
    let e = parse_error("mod m;");
    assert_eq!((e.arg, e.fault, e.at), (Some(Arg::Header), GrammarFault::Missing, 6));
    let e = parse_error(r#"mod m; header "a.h"; define A; define A = 1;"#);
    assert_eq!((e.arg, e.fault, e.at), (Some(Arg::Define), GrammarFault::DefinedTwice, 38));
    let e = parse_error(r#"mod m; frobnicate "a.h";"#);
    assert_eq!((e.arg, e.fault, e.at, e.len), (None, GrammarFault::UnknownArgument, 7, 10));
    let e = parse_error(r#"mod m; header "a.h" link "x";"#);
    assert_eq!((e.arg, e.fault), (Some(Arg::Header), GrammarFault::Expected(crate::grammar::Expected::Semi)));
    let e = parse_error(r#"mod m; header "a.h"; allow "a-b";"#);
    assert_eq!((e.arg, e.fault), (Some(Arg::Allow), GrammarFault::Pattern));
    let e = parse_error(r#"mod m; header "a.h"; link "a/b";"#);
    assert_eq!((e.arg, e.fault), (Some(Arg::Link), GrammarFault::Library));
}

// ------------------------------------------------------------------- the keys

fn key(args: &'static str, toolchain: u8) -> RequestKey {
    let request = request(args);
    RequestKey::of(KeyInputs { request: &request, target: TARGET, dialect: CDialect::Gnu17, toolchain: ContentId([toolchain; 32]) })
}

#[test]
fn the_request_key_changes_with_what_changes_the_record() {
    let base = key(r#"mod m; header "a.h"; header "b.h"; include "x"; include "y"; define A; define B = 2;"#, 1);
    // Defines are a set: their order changes nothing; nor do the module, the selection, the link.
    assert_eq!(base, key(r#"mod m; header "a.h"; header "b.h"; include "x"; include "y"; define B = 2; define A;"#, 1));
    assert_eq!(base, key(r#"define A; mod other; header "a.h"; include "x"; header "b.h"; define B = 2; include "y"; allow "z"; link "q";"#, 1));
    // Headers and include folders are in order: each reorder changes meaning.
    assert_ne!(base, key(r#"mod m; header "b.h"; header "a.h"; include "x"; include "y"; define A; define B = 2;"#, 1));
    assert_ne!(base, key(r#"mod m; header "a.h"; header "b.h"; include "y"; include "x"; define A; define B = 2;"#, 1));
    // A define's value, its sign, the toolchain.
    assert_ne!(base, key(r#"mod m; header "a.h"; header "b.h"; include "x"; include "y"; define A; define B = 3;"#, 1));
    assert_ne!(base, key(r#"mod m; header "a.h"; header "b.h"; include "x"; include "y"; define A; define B = -2;"#, 1));
    assert_ne!(base, key(r#"mod m; header "a.h"; header "b.h"; include "x"; include "y"; define A = 1; define B = 2;"#, 1));
    assert_ne!(base, key(r#"mod m; header "a.h"; header "b.h"; include "x"; include "y"; define A; define B = 2;"#, 2));
}

#[test]
fn the_render_key_holds_what_the_record_key_does_not() {
    let reads = ReadsBuilder::new(heap()).seal().unwrap();
    let r1 = request(r#"mod m; header "a.h";"#);
    let r2 = request(r#"mod n; header "a.h";"#);
    let r3 = request(r#"mod m; header "a.h"; allow "x";"#);
    let k1 = RequestKey::of(KeyInputs { request: &r1, target: TARGET, dialect: CDialect::Gnu17, toolchain: ContentId([1; 32]) });
    let import = ImportKey::of(k1, &reads);
    assert_ne!(RenderKey::of(import, &r1, None), RenderKey::of(import, &r2, None));
    assert_ne!(RenderKey::of(import, &r1, None), RenderKey::of(import, &r3, None));
    assert_eq!(RenderKey::of(import, &r1, None), RenderKey::of(import, &request(r#"mod m; header "a.h";"#), None));
    // The unit the rendering binds to is in it.
    assert_ne!(RenderKey::of(import, &r1, None), RenderKey::of(import, &r1, Some(test_unit())));
}

#[test]
fn bodies_and_macro_fns_key_the_unit_and_the_rendering_never_the_record() {
    let reads = ReadsBuilder::new(heap()).seal().unwrap();
    let base = r#"mod m; header "a.h";"#;
    let asks = r#"mod m; header "a.h"; macro_fn PyArray_NDIM(arr: *const PyArrayObject) -> c_int;"#;
    let off = r#"mod m; header "a.h"; inline_bodies off;"#;
    // The declaration record's key is the same whatever the unit is asked for.
    assert_eq!(key(base, 1), key(asks, 1));
    assert_eq!(key(base, 1), key(off, 1));
    let import = ImportKey::of(key(base, 1), &reads);
    let site = ContentId([7; 32]);
    let (b, a, o) = (request(base), request(asks), request(off));
    let unit = |r: &Request<'_>, site: ContentId| UnitKey::of(import, site, r);
    // The unit's key: each ask, the switch and the site.
    assert_ne!(unit(&b, site), unit(&a, site));
    assert_ne!(unit(&b, site), unit(&o, site));
    assert_ne!(unit(&b, site), unit(&b, ContentId([8; 32])));
    // `on` is the default: saying it is the same request.
    assert_eq!(unit(&b, site), unit(&request(r#"mod m; header "a.h"; inline_bodies on;"#), site));
    // The rendering's key: the asks too.
    assert_ne!(RenderKey::of(import, &b, None), RenderKey::of(import, &a, None));
    assert_ne!(RenderKey::of(import, &b, None), RenderKey::of(import, &o, None));
    // `macro_fn`s are a set: their order is not in a key.
    let two = r#"mod m; header "a.h"; macro_fn B(x: c_int); macro_fn A(p: *mut *const c_char) -> c_long;"#;
    let owt = r#"mod m; header "a.h"; macro_fn A(p: *mut *const c_char) -> c_long; macro_fn B(x: c_int);"#;
    assert_eq!(unit(&request(two), site), unit(&request(owt), site));
    // The symbol is the key's: golden for a fixed key.
    let mut symbol = blaze_own::String::new_in(heap());
    test_unit().symbol("PyArray_NDIM", &mut symbol);
    assert_eq!(symbol.as_str(), "__blaze_c_0102030405060708_PyArray_NDIM");
}

#[test]
fn macro_fn_and_inline_bodies_parse_normalized() {
    let r = request(r#"mod m; header "a.h"; macro_fn B(x: c_int); macro_fn A(p: *mut *const c_char, n: npy_intp) -> c_long;"#);
    assert!(r.inline_bodies);
    let names: StdVec<&str> = r.macro_fns.iter().map(|f| f.name).collect();
    assert_eq!(names, ["A", "B"]);
    let a = r.params_of(&r.macro_fns[0]);
    assert_eq!(a.len(), 2);
    assert_eq!((a[0].name, a[0].ty.name, a[0].ty.depth, a[0].ty.muts), ("p", "c_char", 2, 0b01));
    assert_eq!((a[1].name, a[1].ty.name, a[1].ty.depth), ("n", "npy_intp", 0));
    assert_eq!(r.macro_fns[0].result.map(|t| t.name), Some("c_long"));
    let b = r.params_of(&r.macro_fns[1]);
    assert_eq!((b.len(), b[0].name, r.macro_fns[1].result), (1, "x", None));
    assert!(!request(r#"mod m; header "a.h"; inline_bodies off;"#).inline_bodies);
    let e = parse_error(r#"mod m; header "a.h"; inline_bodies maybe;"#);
    assert_eq!((e.arg, e.fault), (Some(Arg::InlineBodies), GrammarFault::Expected(Expected::Switch)));
    let e = parse_error(r#"mod m; header "a.h"; inline_bodies on; inline_bodies off;"#);
    assert_eq!((e.arg, e.fault), (Some(Arg::InlineBodies), GrammarFault::Repeated));
    let e = parse_error(r#"mod m; header "a.h"; macro_fn A(); macro_fn A(x: c_int);"#);
    assert_eq!((e.arg, e.fault), (Some(Arg::MacroFn), GrammarFault::DefinedTwice));
    let e = parse_error(r#"mod m; header "a.h"; macro_fn A(x: c_void);"#);
    assert_eq!((e.arg, e.fault), (Some(Arg::MacroFn), GrammarFault::Type));
    let e = parse_error(r#"mod m; header "a.h"; macro_fn A(x: *c_int);"#);
    assert_eq!((e.arg, e.fault), (Some(Arg::MacroFn), GrammarFault::Expected(Expected::Type)));
    let e = parse_error(r#"mod m; header "a.h"; macro_fn A(x c_int);"#);
    assert_eq!((e.arg, e.fault), (Some(Arg::MacroFn), GrammarFault::Expected(Expected::Punct(':'))));
}

// ------------------------------------------------------------------- the read list

fn place(root: Root, path: &'static str) -> Place<'static> {
    Place { root, path: RelPath::new(path).unwrap() }
}

#[test]
fn the_read_list_is_sorted_one_a_place_and_holds_misses() {
    let mut reads = ReadsBuilder::new(heap());
    reads.push(place(Root::Shelf, "stddef.h"), Some(b"typedef unsigned long size_t;")).unwrap();
    reads.push(place(Root::Package, "vendor/stddef.h"), None).unwrap();
    reads.push(place(Root::Package, "a.h"), Some(b"int a;")).unwrap();
    reads.push(place(Root::Package, "a.h"), Some(b"int a;")).unwrap();
    let list = reads.seal().unwrap();
    let got: StdVec<(Root, &str, bool)> = list.iter().map(|r| (r.place.root, r.place.path.as_str(), r.digest.is_some())).collect();
    assert_eq!(got, [(Root::Package, "a.h", true), (Root::Package, "vendor/stddef.h", false), (Root::Shelf, "stddef.h", true)]);
    // Its record reads back equal, and says which read today's files change.
    let bytes = list.encode(heap());
    let view = ReadsView::open(&bytes).unwrap();
    assert_eq!(view.len(), 3);
    for (a, b) in view.iter().zip(list.iter()) {
        assert_eq!(a, b);
    }
    let same = |p: Place<'_>| match (p.root, p.path.as_str()) {
        (Root::Package, "a.h") => Some(content_digest(b"int a;")),
        (Root::Shelf, "stddef.h") => Some(content_digest(b"typedef unsigned long size_t;")),
        _ => None,
    };
    assert_eq!(view.first_change(same), None);
    // The miss appearing is a change: a header earlier in the search now answers.
    let appeared = |p: Place<'_>| match p.path.as_str() {
        "vendor/stddef.h" => Some(content_digest(b"")),
        _ => same(p),
    };
    assert_eq!(view.first_change(appeared).map(|r| r.place.path.as_str()), Some("vendor/stddef.h"));
    // The digest of the list is the order-free set's.
    let mut again = ReadsBuilder::new(heap());
    again.push(place(Root::Package, "a.h"), Some(b"int a;")).unwrap();
    again.push(place(Root::Package, "vendor/stddef.h"), None).unwrap();
    again.push(place(Root::Shelf, "stddef.h"), Some(b"typedef unsigned long size_t;")).unwrap();
    assert_eq!(again.seal().unwrap().digest(), list.digest());
}

#[test]
fn a_file_that_changes_during_an_import_is_refused() {
    let mut reads = ReadsBuilder::new(heap());
    reads.push(place(Root::Package, "a.h"), Some(b"int a;")).unwrap();
    reads.push(place(Root::Package, "a.h"), None).unwrap();
    assert!(matches!(reads.seal(), Err(ReadsError::Changed { entry: 0 })));
}

// ------------------------------------------------------------------- the import

struct MapFiles {
    files: StdVec<(Root, &'static str, &'static [u8])>,
    clock: u64,
}

impl Files for MapFiles {
    fn read(&mut self, place: Place<'_>) -> Option<&[u8]> {
        self.files.iter().find(|(root, path, _)| *root == place.root && *path == place.path.as_str()).map(|(_, _, text)| *text)
    }

    fn now(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

const INCLUDES: [ModelInclude<'static>; 1] = [ModelInclude { name: "stddef.h", angled: true }];

#[test]
fn an_import_reads_renders_and_keys_by_its_reads() {
    let request = request(r#"mod rocksdb; header "rocksdb/c.h"; include "vendor"; link "rocksdb";"#);
    let run = |files: StdVec<(Root, &'static str, &'static [u8])>| {
        let front = ModelFrontEnd::new(&INCLUDES, rocksdb, heap());
        let opts = ImportOpts {
            front,
            request: &request,
            target: TARGET,
            dialect: CDialect::Gnu17,
            toolchain: ContentId([1; 32]),
            now: 0,
            alloc: heap(),
        };
        import(opts, &mut MapFiles { files, clock: 0 }, 1000)
    };
    let files = || StdVec::from([(Root::Package, "rocksdb/c.h", &b"/* rocksdb */"[..]), (Root::Shelf, "stddef.h", &b"/* stddef */"[..])]);
    let done = run(files()).expect("the import runs");
    assert_has(done.rust.as_str(), "pub unsafe fn rocksdb_open(");
    assert!(done.findings.is_empty());
    // The read list: the root header, the miss in `vendor`, the shelf's header.
    let reads: StdVec<(Root, &str, bool)> = done.imported.reads.iter().map(|r| (r.place.root, r.place.path.as_str(), r.digest.is_some())).collect();
    assert_eq!(reads, [(Root::Package, "rocksdb/c.h", true), (Root::Package, "vendor/stddef.h", false), (Root::Shelf, "stddef.h", true)]);
    // The same files: the same key and record. Another header text, or the miss filled: another key.
    let again = run(files()).unwrap();
    assert_eq!(again.imported.key, done.imported.key);
    assert_eq!(&again.imported.record[..], &done.imported.record[..]);
    let mut changed = files();
    changed[1].2 = &b"/* stddef, edited */"[..];
    assert_ne!(run(changed).unwrap().imported.key, done.imported.key);
    let mut shadowed = files();
    shadowed.push((Root::Package, "vendor/stddef.h", &b"/* vendored */"[..]));
    assert_ne!(run(shadowed).unwrap().imported.key, done.imported.key);
    // A root header no folder holds: the front end's typed error.
    let missing = run(StdVec::new());
    let error = missing.as_ref().err();
    assert!(matches!(error, Some(ImportError::Front { fault: FrontFault::NotFound, .. })), "{error:?}");
}

// ------------------------------------------------------------------- the differential compare

/// The two sides of one difference: `0` a field's offset, `1` a macro's value, `2` a typedef's
/// type.
fn variant(which: usize, second: bool) -> AVec<'static, u8> {
    let mut b = builder();
    let mut t = Types::new(false);
    match which {
        0 => {
            let int = t.int(&mut b).unwrap();
            let (x, y) = (b.name("x").unwrap(), b.name("y").unwrap());
            let y_at = if second { 64 } else { 32 };
            record(&mut b, "point", body(12, 4, None, &[field(Some(x), int, 0), field(Some(y), int, y_at)], &[])).unwrap();
        }
        1 => {
            let int = t.int(&mut b).unwrap();
            let n = b.name("LEVEL").unwrap();
            let o = origin(&mut b, 0).unwrap();
            b.constant(n, MacroValue::Int { bits: if second { 2 } else { 1 }, ty: int }, o).unwrap();
        }
        _ => {
            let ty = if second { t.long(&mut b).unwrap() } else { t.int(&mut b).unwrap() };
            let n = b.name("t").unwrap();
            let o = origin(&mut b, 0).unwrap();
            b.typedef(n, NsId::GLOBAL, ty, o).unwrap();
        }
    }
    encoded(&b)
}

fn expected(which: usize) -> Difference<'static> {
    match which {
        0 => Difference { table: Table::Records, row: 0, decl: Some("point"), part: Some(Part { index: 1, name: Some("y") }), column: Column::Offset },
        1 => Difference { table: Table::Macros, row: 0, decl: Some("LEVEL"), part: None, column: Column::Value },
        _ => Difference { table: Table::Types, row: 0, decl: None, part: None, column: Column::Extra },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DifferBroken {
    /// The compare did not find the planted difference where it is.
    Missed { which: usize, found: Option<(Table, Column)> },
    /// The compare found a difference between a record and itself.
    SelfDiffers { which: usize },
}

impl fmt::Display for DifferBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

blaze_sim::plants! {
    impl DifferPlant breaks DifferBroken {
        None,
        IgnoresFieldOffsets => DifferBroken::Missed { which: 0, .. },
        IgnoresTypes => DifferBroken::Missed { which: 2, .. },
        IgnoresMacroValues => DifferBroken::Missed { which: 1, .. },
    }
}

fn one_difference(tape: &Tape, plant: impl Planted<DifferPlant>) -> Result<(), Broken<DifferBroken>> {
    let which = tape.index(3, "which");
    let (a, b) = (variant(which, false), variant(which, true));
    let (va, vb) = (DeclsView::open(&a).unwrap(), DeclsView::open(&b).unwrap());
    if first_difference(&va, &va, plant).is_some() {
        return Err(Broken::Oracle(DifferBroken::SelfDiffers { which }));
    }
    let found = first_difference(&va, &vb, plant);
    let want = expected(which);
    match found {
        Some(d) if d.table == want.table && d.row == want.row && d.decl == want.decl && d.part == want.part && d.column == want.column => Ok(()),
        other => Err(Broken::Oracle(DifferBroken::Missed { which, found: other.map(|d| (d.table, d.column)) })),
    }
}

#[test]
fn the_compare_names_the_first_difference() {
    check(Config::cases(48), |tape| one_difference(tape, NoPlant));
}

#[test]
fn every_differ_plant_is_caught() {
    controls(Config::cases(96), |tape, plant: DifferPlant| one_difference(tape, plant));
}

#[test]
fn a_record_and_its_rebuild_do_not_differ() {
    let (mut a, mut b) = (builder(), builder());
    rocksdb(&mut a).unwrap();
    rocksdb(&mut b).unwrap();
    let (ra, rb) = (encoded(&a), encoded(&b));
    let (va, vb) = (DeclsView::open(&ra).unwrap(), DeclsView::open(&rb).unwrap());
    assert_eq!(first_difference(&va, &vb, NoPlant), None);
}
