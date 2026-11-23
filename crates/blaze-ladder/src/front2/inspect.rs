//! Inlay-hints engine for `blaze inspect`: extracts implicit compiler knowledge for any Blaze or
//! Rust source file — inferred return types and `Result<T, _>` error unions, inferred local /
//! pattern / closure binding types (including narrowed union residuals), implicitly defaulted
//! struct/record fields, implicit coercions (`UnionInject`, `UnionWiden`, `RecordProject`,
//! `Unsize`, `ClosureFnPointer`, overloaded `Deref`), `?` propagation error types, and optional
//! expression types — in both human/LLM-friendly annotated source format and deterministic JSON.
//!
//! Strictly iterative (zero recursion), zero panics/stops, zero integer `as` casts, and
//! allocator-parametric (`blaze_own` everywhere outside tests).

use core::alloc::Allocator;
use core::fmt::{self, Write as _};

use blaze_expand::seen::SeenRoot;
use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey as LibKey};
use blaze_lower::scoped::Prepares;
use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};
use blaze_scope::crate_env::{BinNames, CrateEnv, EnvFacts};
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{At, CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::{Cfg, Serial, Target};
use blaze_surface::{Edition as SurfaceEdition, ItemKind};
use blaze_typeck::{
    self as tc, Abi, AdjustKind, BodyView, Checked, Expr, ExprId, FloatTy, FnPtrHeader, IntTy,
    ItemKey, LangItem, Mutbl, Name, PNode, PTy, PTys, Pat, PatId, Safety, Stmt, UintTy,
};
use blaze_unit_graph::{Implicit, Prelude, StdIs};
use edition::{BlazeEdition, Edition as RustEdition, PackageEdition};

use super::Front2;
use super::body::{Body, BodyError, lower_fn, lower_member};
use super::check::{BuildOrder, Records, check_body, eval_len};
use super::consts::{ConstFail, Consts};
use super::heads::{HeadError, Store};
use super::keys::{Keys, MemberKind, Origin, checker_of};
use super::source::NamesSource;
use super::spell::Spelling;
use super::types::Why;
use crate::refusal::SourceFile;

/// Default Blaze edition (`blaze-2026.11`, Rust 2024 syntax + Blaze extensions).
pub const BLAZE_EDITION: PackageEdition = PackageEdition {
    syntax: RustEdition::Edition2024,
    blaze: Some(BlazeEdition::Blaze2026_11),
};

/// Standard Rust 2024 edition.
pub const RUST_2024: PackageEdition = PackageEdition {
    syntax: RustEdition::Edition2024,
    blaze: None,
};

/// Upper bound on type nodes expanded by [`format_ty_in`] (prevents runaway output).
const MOST_NODES: usize = 128;

/// Upper bound on iterative AST / pattern / type worklist steps.
const MOST_WALK_STEPS: usize = 65_536;

/// Maximum constant-length evaluations per function body before giving up.
const MOST_LENS: u32 = 8;

/// Synthetic root file path used when resolving a single in-memory source text.
const INSPECT_ROOT: &str = "/inspect/src/lib.rs";

/// Options controlling which inlay hints are extracted for each function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InspectOpts {
    /// Emit hints for implicit coercions (`UnionInject`, `UnionWiden`, `RecordProject`, etc.).
    pub coercions: bool,
    /// Emit hints listing implicitly defaulted fields on struct/record literals.
    pub defaults: bool,
    /// Emit hints showing the propagated error type at `?` sites.
    pub try_sites: bool,
    /// Emit hints showing inferred types for unannotated bindings (locals, patterns, closures).
    pub bindings: bool,
    /// Emit expression type hints for non-trivial expressions (most useful with `--json`).
    pub all_exprs: bool,
}

impl Default for InspectOpts {
    fn default() -> Self {
        Self {
            coercions: true,
            defaults: true,
            try_sites: true,
            bindings: true,
            all_exprs: false,
        }
    }
}

/// Classification of an extracted inlay hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HintKind {
    InferredReturn,
    Binding,
    DefaultFields,
    Coercion,
    TryPropagate,
    ExprType,
}

impl HintKind {
    /// Stable lowercase identifier used in JSON output.
    pub const fn as_str(self) -> &'static str {
        match self {
            HintKind::InferredReturn => "inferred_return",
            HintKind::Binding => "binding",
            HintKind::DefaultFields => "default_fields",
            HintKind::Coercion => "coercion",
            HintKind::TryPropagate => "try_propagate",
            HintKind::ExprType => "expr_type",
        }
    }
}

/// One inlay hint attached to an exact byte offset in the inspected source file.
#[derive(Debug, PartialEq, Eq)]
pub struct Hint<A: Allocator> {
    /// 1-based line number in the inspected source file.
    pub line: u32,
    /// 1-based UTF-8 character column on `line`.
    pub col: u32,
    /// Exact byte offset in `InspectOutput::source` where the hint attaches.
    pub byte_offset: usize,
    /// Category of the hint.
    pub kind: HintKind,
    /// Hint body text (without surrounding `/*` / `*/`).
    pub label: OwnString<A>,
}

/// Inspected summary and inlay hints for one function or method item.
#[derive(Debug)]
pub struct InspectedItem<A: Allocator> {
    /// Function or method display name (e.g. `load` or `Type::method`).
    pub name: OwnString<A>,
    /// Compact resolved signature header (e.g. `fn load(x: u32) -> Result<u32, ErrA | ErrB>`).
    pub signature: OwnString<A>,
    /// Concrete inferred return type when the signature wrote a `_` hole.
    pub inferred_output: Option<OwnString<A>>,
    /// Concrete inferred error union when the signature wrote `Result<T, _>`.
    pub inferred_error_set: Option<OwnString<A>>,
    /// 1-based starting line of the item in the source file.
    pub line: u32,
    /// 1-based ending line of the item in the source file (inclusive).
    pub end_line: u32,
    /// Starting byte offset of the item in `InspectOutput::source`.
    pub byte_start: usize,
    /// Ending byte offset of the item in `InspectOutput::source`.
    pub byte_end: usize,
    /// Deterministic, sorted inlay hints for this item.
    pub hints: OwnVec<Hint<A>, A>,
}

/// Complete inspection output for a source file.
#[derive(Debug)]
pub struct InspectOutput<A: Allocator> {
    pub source: OwnString<A>,
    pub items: OwnVec<InspectedItem<A>, A>,
}

/// Typed failure returned by [`inspect_source`].
#[derive(Debug)]
pub enum InspectError {
    /// Could not allocate an inference memory frame.
    Frame(blaze_mem::MemError),
    /// Could not initialize the synthetic crate environment.
    CrateEnv,
    /// Name resolution of the source file failed.
    Resolve { message: OwnString<Alloc<'static>> },
    /// Front2 initialization failed.
    Front(super::FrontError),
    /// The requested `::item` filter did not match any function in the file.
    ItemNotFound,
    /// Checking failed for a function and produced no inspectable items.
    CheckFailed { message: OwnString<Alloc<'static>> },
}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InspectError::Frame(e) => write!(f, "inspect: failed to allocate frame: {e}"),
            InspectError::CrateEnv => f.write_str("inspect: failed to initialize crate environment"),
            InspectError::Resolve { message } => write!(f, "inspect: name resolution failed: {}", message.as_str()),
            InspectError::Front(e) => write!(f, "inspect: front-end setup failed: {e}"),
            InspectError::ItemNotFound => f.write_str("inspect: no matching function found for filter"),
            InspectError::CheckFailed { message } => write!(f, "inspect: {message}"),
        }
    }
}

impl core::error::Error for InspectError {}

/// Mapping from `blaze_typeck::Name` hashes back to identifier text collected from the source
/// file and body lowering tables.
pub struct NameTable<'s, A: Allocator> {
    by_name: HashMap<Name, &'s str, A>,
}

impl<'s, A: Allocator> NameTable<'s, A> {
    /// Inserts an explicit `(Name, &str)` entry.
    pub fn insert(&mut self, name: Name, text: &'s str) {
        self.by_name.insert(name, text);
    }

    /// Looks up the identifier spelling for `name`.
    pub fn get(&self, name: Name) -> Option<&'s str> {
        self.by_name.get(&name).copied()
    }
}

impl<'s, A: Allocator + Copy> NameTable<'s, A> {
    /// Scans all identifier tokens in `source` and hashes them into a lookup table so record
    /// field names, struct field names, and generic parameter names resolve even when declared in
    /// another item (type aliases, structs, signatures).
    pub fn from_source(source: &'s str, alloc: A) -> Self {
        let mut by_name = HashMap::new_in(alloc);
        let bytes = source.as_bytes();
        let len = bytes.len();
        let mut i = 0usize;
        while i < len {
            let Some(&b) = bytes.get(i) else { break };
            if b.is_ascii_alphabetic() || b == b'_' {
                let start = i;
                i += 1;
                while let Some(&c) = bytes.get(i) {
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                if let Some(ident) = source.get(start..i) {
                    // Handle raw identifier `r#ident`: when `ident == "r"` followed by `#<ident>`.
                    if ident == "r" && bytes.get(i) == Some(&b'#') {
                        let r_start = i + 1;
                        let mut j = r_start;
                        while let Some(&c) = bytes.get(j) {
                            if c.is_ascii_alphanumeric() || c == b'_' {
                                j += 1;
                            } else {
                                break;
                            }
                        }
                        if j > r_start
                            && let Some(raw_ident) = source.get(r_start..j)
                        {
                            by_name.insert(Name::of(raw_ident), raw_ident);
                            i = j;
                            continue;
                        }
                    }
                    by_name.insert(Name::of(ident), ident);
                }
            } else {
                i += 1;
            }
        }
        Self { by_name }
    }
}

/// Context passed to [`format_ty_in`] for resolving item names, field spellings, and generic
/// parameter names.
pub struct TypeFmtCx<'a, 'n, S: NamesSource<'n>, A: Allocator> {
    pub names: S,
    pub keys: &'a Keys,
    pub store: &'a Store<'n>,
    pub name_table: &'a NameTable<'a, A>,
    pub spelling: Option<&'a Spelling<'a>>,
    pub generic_params: &'a [(Name, u32)],
}

impl<'a, 'n, S: NamesSource<'n>, A: Allocator> TypeFmtCx<'a, 'n, S, A> {
    fn spell(&self, name: Name) -> Option<&str> {
        if let Some(s) = self.spelling
            && let Ok(text) = s.get(name)
        {
            return Some(text);
        }
        self.name_table.get(name)
    }

    fn param_name(&self, idx: u32) -> Option<&str> {
        for &(n, i) in self.generic_params {
            if i == idx {
                return self.spell(n);
            }
        }
        None
    }

    fn write_item_short<B: Allocator>(&self, key: ItemKey, out: &mut OwnString<B>) {
        if let Some(Origin::Member { container, name, .. }) = self.keys.origin(key) {
            if let Some(c_lib) = self.keys.library_in(self.names, *container)
                && let Some(c_name) = self.names.name_of(c_lib)
            {
                out.push_str(c_name);
                out.push_str("::");
            }
            out.push_str(name);
            return;
        }
        if let Some(lib) = self.keys.library_in(self.names, key) {
            if let Some((adt, _)) = self.names.variant_of(lib) {
                if let Some(adt_name) = self.names.name_of(adt) {
                    out.push_str(adt_name);
                    out.push_str("::");
                }
                if let Some(var_name) = self.names.name_of(lib) {
                    out.push_str(var_name);
                    return;
                }
            }
            if let Some(item_name) = self.names.name_of(lib) {
                out.push_str(item_name);
                return;
            }
        }
        let bytes = key.0.to_be_bytes();
        let _ = write!(
            out,
            "Item#{:02x}{:02x}{:02x}",
            bytes.get(13).copied().unwrap_or(0),
            bytes.get(14).copied().unwrap_or(0),
            bytes.get(15).copied().unwrap_or(0)
        );
    }
}

/// Worklist item for iterative [`format_ty_in`].
enum FmtOp {
    Ty(PTy),
    Static(&'static str),
    Name { name: Name, fallback_index: usize },
    Item(ItemKey),
}

/// Iteratively formats a portable type `root` from `tys` into `out`.
///
/// Zero recursion, bounded by [`MOST_NODES`]. Handles every [`PNode`] variant cleanly, including
/// structural records (`{ a: u32, b: bool }`), structural unions (`ErrA | ErrB`), ADTs with
/// generic arguments (`Result<u32, ErrA | ErrB>`), tuples, references, pointers, slices, arrays,
/// fn pointers, closures, futures, projections, trait objects, and const values.
pub fn format_ty_in<'a, 'n, S, A, B>(
    tys: PTys<'_>,
    root: PTy,
    cx: &TypeFmtCx<'a, 'n, S, A>,
    out: &mut OwnString<B>,
    scratch: B,
) where
    S: NamesSource<'n>,
    A: Allocator,
    B: Allocator + Copy,
{
    let mut stack: OwnVec<FmtOp, B> = OwnVec::with_capacity_in(32, scratch);
    stack.push(FmtOp::Ty(root));
    let mut expanded = 0usize;

    while let Some(op) = stack.pop() {
        match op {
            FmtOp::Static(s) => out.push_str(s),
            FmtOp::Item(k) => cx.write_item_short(k, out),
            FmtOp::Name { name, fallback_index } => match cx.spell(name) {
                Some(s) => out.push_str(s),
                None => {
                    let _ = write!(out, "_{fallback_index}");
                }
            },
            FmtOp::Ty(ty) => {
                if expanded >= MOST_NODES {
                    out.push_str("..");
                    continue;
                }
                expanded += 1;
                let Ok(node) = tys.node(ty) else {
                    out.push_str("{unknown}");
                    continue;
                };
                match node {
                    PNode::Bool => out.push_str("bool"),
                    PNode::Char => out.push_str("char"),
                    PNode::Str => out.push_str("str"),
                    PNode::Never => out.push_str("!"),
                    PNode::Infer => out.push_str("_"),
                    PNode::Error => out.push_str("{error}"),
                    PNode::Int(i) => out.push_str(int_ty_str(i)),
                    PNode::Uint(u) => out.push_str(uint_ty_str(u)),
                    PNode::Float(f) => out.push_str(float_ty_str(f)),
                    PNode::Ref(mutbl, inner) => {
                        out.push_str(match mutbl {
                            Mutbl::Not => "&",
                            Mutbl::Mut => "&mut ",
                        });
                        stack.push(FmtOp::Ty(inner));
                    }
                    PNode::Ptr(mutbl, inner) => {
                        out.push_str(match mutbl {
                            Mutbl::Not => "*const ",
                            Mutbl::Mut => "*mut ",
                        });
                        stack.push(FmtOp::Ty(inner));
                    }
                    PNode::Slice(inner) => {
                        out.push('[');
                        stack.push(FmtOp::Static("]"));
                        stack.push(FmtOp::Ty(inner));
                    }
                    PNode::Array(elem, len) => {
                        out.push('[');
                        stack.push(FmtOp::Static("]"));
                        stack.push(FmtOp::Ty(len));
                        stack.push(FmtOp::Static("; "));
                        stack.push(FmtOp::Ty(elem));
                    }
                    PNode::Tuple(list) => {
                        let Ok(elems) = tys.list(list) else {
                            out.push_str("()");
                            continue;
                        };
                        if elems.is_empty() {
                            out.push_str("()");
                        } else if elems.len() == 1 {
                            if let Some(&first) = elems.first() {
                                out.push('(');
                                stack.push(FmtOp::Static(",)"));
                                stack.push(FmtOp::Ty(first));
                            }
                        } else {
                            out.push('(');
                            stack.push(FmtOp::Static(")"));
                            push_joined(&mut stack, elems, ", ");
                        }
                    }
                    PNode::Record(schema, list) => {
                        let Ok(fields) = tys.list(list) else {
                            out.push_str("{}");
                            continue;
                        };
                        if fields.is_empty() {
                            out.push_str("{}");
                            continue;
                        }
                        let names = tys.record_names(schema);
                        out.push_str("{ ");
                        stack.push(FmtOp::Static(" }"));
                        for idx in (0..fields.len()).rev() {
                            if idx + 1 < fields.len() {
                                stack.push(FmtOp::Static(", "));
                            }
                            if let Some(&field_ty) = fields.get(idx) {
                                stack.push(FmtOp::Ty(field_ty));
                                stack.push(FmtOp::Static(": "));
                                match names.and_then(|ns| ns.get(idx).copied()) {
                                    Some(name) => stack.push(FmtOp::Name {
                                        name,
                                        fallback_index: idx,
                                    }),
                                    None => {
                                        stack.push(FmtOp::Name {
                                            name: Name(0),
                                            fallback_index: idx,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    PNode::Union(list) => {
                        let Ok(members) = tys.list(list) else {
                            out.push_str("!");
                            continue;
                        };
                        if members.is_empty() {
                            out.push_str("!");
                        } else {
                            push_joined(&mut stack, members, " | ");
                        }
                    }
                    PNode::Adt(key, args) => {
                        cx.write_item_short(key, out);
                        if let Ok(arg_slice) = tys.list(args)
                            && !arg_slice.is_empty()
                        {
                            out.push('<');
                            stack.push(FmtOp::Static(">"));
                            push_joined(&mut stack, arg_slice, ", ");
                        }
                    }
                    PNode::FnDef(key, args) => {
                        out.push_str("fn ");
                        cx.write_item_short(key, out);
                        if let Ok(arg_slice) = tys.list(args)
                            && !arg_slice.is_empty()
                        {
                            out.push_str("::<");
                            stack.push(FmtOp::Static(">"));
                            push_joined(&mut stack, arg_slice, ", ");
                        }
                    }
                    PNode::FnPtr(list, header) => {
                        write_fn_ptr_header(header, out);
                        out.push_str("fn(");
                        if let Ok(items) = tys.list(list)
                            && let Some((&ret, inputs)) = items.split_last()
                        {
                            let is_unit = matches!(
                                tys.node(ret),
                                Ok(PNode::Tuple(l)) if l.len == 0
                            );
                            if !is_unit {
                                stack.push(FmtOp::Ty(ret));
                                stack.push(FmtOp::Static(" -> "));
                            }
                            if header.c_variadic {
                                stack.push(FmtOp::Static(if inputs.is_empty() {
                                    "...)"
                                } else {
                                    ", ...)"
                                }));
                            } else {
                                stack.push(FmtOp::Static(")"));
                            }
                            push_joined(&mut stack, inputs, ", ");
                        } else {
                            out.push(')');
                        }
                    }
                    PNode::Closure { ordinal, .. } => {
                        let _ = write!(out, "{{closure#{ordinal}}}");
                    }
                    PNode::Future(_, output, _) => {
                        out.push_str("impl Future<Output = ");
                        stack.push(FmtOp::Static(">"));
                        stack.push(FmtOp::Ty(output));
                    }
                    PNode::Coroutine { ordinal, .. } => {
                        let _ = write!(out, "{{coroutine#{ordinal}}}");
                    }
                    PNode::Param(idx) => match cx.param_name(idx) {
                        Some(name) => out.push_str(name),
                        None => {
                            let _ = write!(out, "T{idx}");
                        }
                    },
                    PNode::ConstParam(idx) => match cx.param_name(idx) {
                        Some(name) => out.push_str(name),
                        None => {
                            let _ = write!(out, "N{idx}");
                        }
                    },
                    PNode::ConstValue { ty: cty, bits } => {
                        format_const_value(tys, cty, bits, out);
                    }
                    PNode::Opaque(key, args) => {
                        out.push_str("impl ");
                        cx.write_item_short(key, out);
                        if let Ok(arg_slice) = tys.list(args)
                            && !arg_slice.is_empty()
                        {
                            out.push('<');
                            stack.push(FmtOp::Static(">"));
                            push_joined(&mut stack, arg_slice, ", ");
                        }
                    }
                    PNode::Alias { trait_, name, args } => {
                        if let Ok(arg_slice) = tys.list(args)
                            && let Some((&self_ty, rest)) = arg_slice.split_first()
                        {
                            out.push('<');
                            stack.push(FmtOp::Name { name, fallback_index: 0 });
                            stack.push(FmtOp::Static(">::"));
                            if !rest.is_empty() {
                                stack.push(FmtOp::Static(">"));
                                push_joined(&mut stack, rest, ", ");
                                stack.push(FmtOp::Static("<"));
                            }
                            stack.push(FmtOp::Item(trait_));
                            stack.push(FmtOp::Static(" as "));
                            stack.push(FmtOp::Ty(self_ty));
                        } else {
                            cx.write_item_short(trait_, out);
                            out.push_str("::");
                            stack.push(FmtOp::Name { name, fallback_index: 0 });
                        }
                    }
                    PNode::Dynamic(list) => {
                        out.push_str("dyn ");
                        if let Ok(preds) = tys.list(list) {
                            push_joined(&mut stack, preds, " + ");
                        }
                    }
                    PNode::ExTrait(key, args) => {
                        cx.write_item_short(key, out);
                        if let Ok(arg_slice) = tys.list(args)
                            && !arg_slice.is_empty()
                        {
                            out.push('<');
                            stack.push(FmtOp::Static(">"));
                            push_joined(&mut stack, arg_slice, ", ");
                        }
                    }
                    PNode::ExProj { name, term, .. } => {
                        stack.push(FmtOp::Ty(term));
                        stack.push(FmtOp::Static(" = "));
                        stack.push(FmtOp::Name { name, fallback_index: 0 });
                    }
                    PNode::ExAuto(key) | PNode::Foreign(key) => {
                        cx.write_item_short(key, out);
                    }
                    PNode::Pat(base, _) => {
                        stack.push(FmtOp::Static(" is .."));
                        stack.push(FmtOp::Ty(base));
                    }
                }
            }
        }
    }
}

fn push_joined<B: Allocator>(stack: &mut OwnVec<FmtOp, B>, items: &[PTy], sep: &'static str) {
    for idx in (0..items.len()).rev() {
        if idx + 1 < items.len() {
            stack.push(FmtOp::Static(sep));
        }
        if let Some(&ty) = items.get(idx) {
            stack.push(FmtOp::Ty(ty));
        }
    }
}

const fn int_ty_str(i: IntTy) -> &'static str {
    match i {
        IntTy::Isize => "isize",
        IntTy::I8 => "i8",
        IntTy::I16 => "i16",
        IntTy::I32 => "i32",
        IntTy::I64 => "i64",
        IntTy::I128 => "i128",
    }
}

const fn uint_ty_str(u: UintTy) -> &'static str {
    match u {
        UintTy::Usize => "usize",
        UintTy::U8 => "u8",
        UintTy::U16 => "u16",
        UintTy::U32 => "u32",
        UintTy::U64 => "u64",
        UintTy::U128 => "u128",
    }
}

const fn float_ty_str(f: FloatTy) -> &'static str {
    match f {
        FloatTy::F16 => "f16",
        FloatTy::F32 => "f32",
        FloatTy::F64 => "f64",
        FloatTy::F128 => "f128",
    }
}

fn write_fn_ptr_header<B: Allocator>(header: FnPtrHeader, out: &mut OwnString<B>) {
    if header.safety == Safety::Unsafe {
        out.push_str("unsafe ");
    }
    match header.abi {
        Abi::Rust => {}
        Abi::C => out.push_str("extern \"C\" "),
        Abi::CUnwind => out.push_str("extern \"C-unwind\" "),
        Abi::System => out.push_str("extern \"system\" "),
        Abi::SystemUnwind => out.push_str("extern \"system-unwind\" "),
        Abi::RustCall => out.push_str("extern \"rust-call\" "),
        Abi::LlvmIntrinsic => out.push_str("extern \"llvm-intrinsic\" "),
        other => {
            let _ = write!(out, "extern \"{other:?}\" ");
        }
    }
}

fn format_const_value<B: Allocator>(tys: PTys<'_>, ty: PTy, bits: u128, out: &mut OwnString<B>) {
    match tys.node(ty) {
        Ok(PNode::Bool) => out.push_str(if bits != 0 { "true" } else { "false" }),
        Ok(PNode::Char) => {
            if let Ok(code) = u32::try_from(bits)
                && let Some(ch) = char::from_u32(code)
            {
                let _ = write!(out, "'{ch}'");
            } else {
                let _ = write!(out, "{bits}");
            }
        }
        Ok(PNode::Int(i)) => {
            let signed = match i {
                IntTy::I8 => i128::from(i8::from_le_bytes([bits.to_le_bytes()[0]])),
                IntTy::I16 => {
                    let b = bits.to_le_bytes();
                    i128::from(i16::from_le_bytes([b[0], b[1]]))
                }
                IntTy::I32 => {
                    let b = bits.to_le_bytes();
                    i128::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                }
                IntTy::I64 | IntTy::Isize => {
                    let b = bits.to_le_bytes();
                    i128::from(i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
                }
                IntTy::I128 => i128::from_le_bytes(bits.to_le_bytes()),
            };
            let _ = write!(out, "{signed}");
        }
        _ => {
            let _ = write!(out, "{bits}");
        }
    }
}

/// Precomputed line start byte offsets for fast 1-based `(line, col)` lookup.
struct LineIndex<A: Allocator> {
    starts: OwnVec<usize, A>,
}

impl<A: Allocator + Copy> LineIndex<A> {
    fn new(source: &str, alloc: A) -> Self {
        let mut starts = OwnVec::new_in(alloc);
        starts.push(0);
        for (i, b) in source.as_bytes().iter().enumerate() {
            if *b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self { starts }
    }

    /// Returns 1-based `(line, col)` where `col` counts UTF-8 scalar values on the line.
    fn line_col(&self, source: &str, byte_offset: usize) -> (u32, u32) {
        let clamped = clamp_char_boundary(source, byte_offset);
        let idx = self.starts.partition_point(|&s| s <= clamped).saturating_sub(1);
        let line_start = self.starts.get(idx).copied().unwrap_or(0);
        let col_chars = source.get(line_start..clamped).map_or(0, |s| s.chars().count());
        let line = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let col = u32::try_from(col_chars + 1).unwrap_or(u32::MAX);
        (line, col)
    }
}

/// Snaps `offset` down to the nearest valid UTF-8 char boundary in `s`.
fn clamp_char_boundary(s: &str, offset: usize) -> usize {
    let mut at = offset.min(s.len());
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Iteratively checks whether `root` in `tys` contains any `PNode::Infer` hole.
fn has_infer_hole<A: Allocator + Copy>(tys: PTys<'_>, root: PTy, alloc: A) -> bool {
    let mut stack: OwnVec<PTy, A> = OwnVec::with_capacity_in(16, alloc);
    stack.push(root);
    let mut steps = 0usize;
    while let Some(ty) = stack.pop() {
        steps += 1;
        if steps > MOST_WALK_STEPS {
            return false;
        }
        let Ok(node) = tys.node(ty) else { continue };
        if matches!(node, PNode::Infer) {
            return true;
        }
        let _ = tys.children_of(node, &mut stack);
    }
    false
}

/// Finds the byte offset right after the `_` hole in a function's return type header.
///
/// When `error_arg_only` is `true` (e.g. `-> Result<u32, _>`), targets the `_` inside the second
/// generic argument after `,`; otherwise targets the first standalone `_` after `->`.
fn find_return_hole_offset(item_text: &str, header_end: usize, error_arg_only: bool) -> Option<usize> {
    let header = item_text.get(..header_end.min(item_text.len()))?;
    let bytes = header.as_bytes();
    // Locate top-level `->` outside `()`, `<>`, `[]`, `{}`.
    let mut paren = 0u32;
    let mut angle = 0u32;
    let mut bracket = 0u32;
    let mut brace = 0u32;
    let mut arrow_end = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(&b) = bytes.get(i) else { break };
        match b {
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'<' => angle += 1,
            b'>' => angle = angle.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b'{' => brace += 1,
            b'}' => brace = brace.saturating_sub(1),
            b'-' if paren == 0
                && angle == 0
                && bracket == 0
                && brace == 0
                && bytes.get(i + 1) == Some(&b'>') =>
            {
                arrow_end = Some(i + 2);
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let ret_start = arrow_end?;
    let ret_slice = header.get(ret_start..)?;
    let ret_bytes = ret_slice.as_bytes();

    // Stop before a `where` clause if present.
    let mut scan_end = ret_bytes.len();
    let mut p = 0usize;
    while p + 5 <= ret_bytes.len() {
        if ret_slice.get(p..p + 5) == Some("where") {
            let before_ok = p == 0
                || ret_bytes
                    .get(p - 1)
                    .is_some_and(|c| !c.is_ascii_alphanumeric() && *c != b'_');
            let after_ok = ret_bytes
                .get(p + 5)
                .is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'_');
            if before_ok && after_ok {
                scan_end = p;
                break;
            }
        }
        p += 1;
    }

    let mut seen_comma_in_angle = !error_arg_only;
    let mut a_depth = 0u32;
    let mut j = 0usize;
    while j < scan_end {
        let Some(&b) = ret_bytes.get(j) else { break };
        match b {
            b'<' => a_depth += 1,
            b'>' => a_depth = a_depth.saturating_sub(1),
            b',' if a_depth == 1 => seen_comma_in_angle = true,
            b'_' if seen_comma_in_angle => {
                let prev_ok = j == 0
                    || ret_bytes
                        .get(j - 1)
                        .is_some_and(|c| !c.is_ascii_alphanumeric() && *c != b'_');
                let next_ok = ret_bytes
                    .get(j + 1)
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'_');
                if prev_ok && next_ok {
                    return Some(ret_start + j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Finds the byte offset of the function body's opening `{` inside `item_text`, skipping nested
/// braces inside record return types (like `-> { a: u32 } { ... }`) and attributes.
fn find_body_brace(item_text: &str) -> usize {
    let bytes = item_text.as_bytes();
    let mut paren = 0u32;
    let mut bracket = 0u32;
    let mut angle = 0u32;
    let mut seen_fn_params = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(&b) = bytes.get(i) else { break };
        match b {
            b'(' => paren += 1,
            b')' => {
                paren = paren.saturating_sub(1);
                if paren == 0 && bracket == 0 {
                    seen_fn_params = true;
                }
            }
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b'<' => angle += 1,
            b'>' => angle = angle.saturating_sub(1),
            b'{' if seen_fn_params && paren == 0 && bracket == 0 && angle == 0 => {
                // Distinguish a record return type `{ field: T }` from the body block `{ ... }`
                // by checking if another `{` follows after the matching `}`.
                if let Some(close) = find_matching_brace(bytes, i) {
                    let rest = item_text.get(close + 1..).unwrap_or("").trim_start();
                    if rest.starts_with('{') || rest.starts_with("where") {
                        i = close + 1;
                        continue;
                    }
                }
                return i;
            }
            _ => {}
        }
        i += 1;
    }
    item_text.len()
}

fn find_matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0u32;
    let mut i = open;
    while i < bytes.len() {
        match bytes.get(i)? {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Checks whether a `Pat::Bind` at `pat_text` actually corresponds to a user-written binding for
/// `ident` (filtering out hygienic bindings introduced by `?` / `for` desugarings) and returns
/// the item-relative byte offset immediately after the bound identifier.
fn binding_hint_offset(pat_text: &str, pat_start: usize, ident: &str, has_subpat: bool) -> Option<usize> {
    let trimmed_end = pat_text.trim_end();
    if trimmed_end.is_empty() {
        return None;
    }
    if has_subpat {
        // Form `[ref] [mut] ident @ subpat`: find `ident` before `@`.
        let before_at = trimmed_end.split('@').next()?.trim_end();
        if ident_matches_tail(before_at, ident) {
            return Some(pat_start + before_at.len());
        }
        return None;
    }
    if ident_matches_tail(trimmed_end, ident) {
        return Some(pat_start + trimmed_end.len());
    }
    None
}

fn ident_matches_tail(text: &str, ident: &str) -> bool {
    let Some(prefix) = text.strip_suffix(ident) else {
        return false;
    };
    if prefix.is_empty() {
        return true;
    }
    if let Some(before_raw) = prefix.strip_suffix("r#") {
        return before_raw.is_empty()
            || before_raw
                .as_bytes()
                .last()
                .is_some_and(|c| !c.is_ascii_alphanumeric() && *c != b'_');
    }
    prefix
        .as_bytes()
        .last()
        .is_some_and(|c| !c.is_ascii_alphanumeric() && *c != b'_')
}

/// Marks `root_pat` and all of its descendant pattern IDs in `marked` iteratively.
fn mark_pat_tree<A: Allocator + Copy>(
    body: &BodyView<'_>,
    root_pat: PatId,
    marked: &mut [bool],
    alloc: A,
) {
    let mut stack: OwnVec<PatId, A> = OwnVec::with_capacity_in(16, alloc);
    stack.push(root_pat);
    let mut steps = 0usize;
    while let Some(pid) = stack.pop() {
        steps += 1;
        if steps > MOST_WALK_STEPS {
            break;
        }
        let Some(slot) = marked.get_mut(pid.ix()) else { continue };
        if *slot {
            continue;
        }
        *slot = true;
        let Some(&pat) = body.pats.get(pid.ix()) else { continue };
        match pat {
            Pat::Bind { sub: Some(sub), .. } | Pat::Ref { inner: sub, .. } => stack.push(sub),
            Pat::Tuple { elems, .. }
            | Pat::TupleStruct { elems, .. }
            | Pat::Slice { elems, .. } => {
                if let Some(slice) = body.pat_lists.get(elems.range()) {
                    for &child in slice {
                        stack.push(child);
                    }
                }
            }
            Pat::Or { alts } => {
                if let Some(slice) = body.pat_lists.get(alts.range()) {
                    for &child in slice {
                        stack.push(child);
                    }
                }
            }
            Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                if let Some(slice) = body.field_pats.get(fields.range()) {
                    for fp in slice {
                        stack.push(fp.pat);
                    }
                }
            }
            Pat::Range { lo, hi, .. } => {
                if let Some(p) = lo {
                    stack.push(p);
                }
                if let Some(p) = hi {
                    stack.push(p);
                }
            }
            Pat::Missing | Pat::Wild | Pat::Bind { sub: None, .. } | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) => {}
        }
    }
}

/// Extracts all inlay hints for one checked function body.
#[expect(clippy::too_many_arguments, reason = "explicit capability and table inputs without global state")]
pub fn collect_body_hints<'a, 'n, S, A>(
    full_source: &str,
    item_text: &str,
    item_byte_start: usize,
    owner: ItemKey,
    body_view: &BodyView<'_>,
    checked: &Checked<'_>,
    cx: &TypeFmtCx<'a, 'n, S, A>,
    opts: InspectOpts,
    alloc: A,
) -> OwnVec<Hint<A>, A>
where
    S: NamesSource<'n>,
    A: Allocator + Copy,
{
    let lines = LineIndex::new(full_source, alloc);
    let mut hints: OwnVec<Hint<A>, A> = OwnVec::new_in(alloc);
    let checked_tys = checked.types.view();
    let store_tys = cx.store.tys.view();

    let push_hint = |hints: &mut OwnVec<Hint<A>, A>, abs_offset: usize, kind: HintKind, label: OwnString<A>| {
        let clamped = clamp_char_boundary(full_source, abs_offset);
        let (line, col) = lines.line_col(full_source, clamped);
        hints.push(Hint {
            line,
            col,
            byte_offset: clamped,
            kind,
            label,
        });
    };

    // 1. Inferred return type / `Result<T, _>` error union hint in the function header.
    if let Some(ret_ty) = cx.store.inferred_return_type(owner) {
        let header_end = cx
            .store
            .fns
            .get(&owner)
            .and_then(|h| h.text_prefix)
            .and_then(|p| usize::try_from(p).ok())
            .unwrap_or_else(|| find_body_brace(item_text));

        let is_result_err_hole = is_result_error_hole(cx.store, owner, alloc);
        let err_ty = if is_result_err_hole {
            cx.store.inferred_error_set(owner)
        } else {
            None
        };

        let hole_rel = match err_ty {
            Some(_) => find_return_hole_offset(item_text, header_end, true)
                .or_else(|| find_return_hole_offset(item_text, header_end, false)),
            None => find_return_hole_offset(item_text, header_end, false),
        };
        if let Some(rel) = hole_rel {
            let mut label = OwnString::from_str_in("= ", alloc);
            format_ty_in(store_tys, err_ty.unwrap_or(ret_ty), cx, &mut label, alloc);
            push_hint(&mut hints, item_byte_start + rel, HintKind::InferredReturn, label);
        }
    }

    // Track synthetic try-desugaring internal expressions so we avoid noisy hints on generated
    // `Try::branch` / `Try::from_residual` / `ControlFlow` nodes.
    let mut synthetic_expr: OwnVec<bool, A> = OwnVec::with_capacity_in(body_view.exprs.len(), alloc);
    synthetic_expr.resize(body_view.exprs.len(), false);

    // 2. Try propagation (`?`) sites: detect `Match { scrutinee: Call(TryTraitBranch, [inner]), arms }`.
    for (idx, expr) in body_view.exprs.iter().enumerate() {
        let Expr::Match { scrutinee, arms } = *expr else { continue };
        let Some(Expr::Call { callee, args }) = body_view.exprs.get(scrutinee.ix()).copied() else {
            continue;
        };
        let Some(Expr::Path(path_idx)) = body_view.exprs.get(callee.ix()).copied() else {
            continue;
        };
        let Some(path) = body_view.paths.get(path_idx.ix()) else { continue };
        if path.lang != Some(LangItem::TryTraitBranch) {
            continue;
        }
        // Mark the synthetic wrapper expressions (but NOT the user's inner operand).
        if let Some(s) = synthetic_expr.get_mut(scrutinee.ix()) {
            *s = true;
        }
        if let Some(s) = synthetic_expr.get_mut(callee.ix()) {
            *s = true;
        }
        if let Some(arm_slice) = body_view.arms.get(arms.range()) {
            for arm in arm_slice {
                mark_expr_shallow_synthetic(body_view, arm.body, &mut synthetic_expr, alloc);
            }
        }
        if !opts.try_sites {
            continue;
        }
        let Some(match_range) = body_view.expr_ranges.get(idx).copied() else { continue };
        let (Ok(start), Ok(end)) = (usize::try_from(match_range.start), usize::try_from(match_range.end)) else {
            continue;
        };
        if item_text.get(start..end).is_none_or(|s| !s.trim_end().ends_with('?')) {
            continue;
        }
        let Some(arg_slice) = body_view.expr_lists.get(args.range()) else { continue };
        let Some(&inner_expr) = arg_slice.first() else { continue };
        let Some(&inner_ty) = checked.expr_ty.get(inner_expr.ix()) else { continue };
        let Ok(PNode::Adt(_, ty_args)) = checked_tys.node(inner_ty) else { continue };
        let Ok(ty_arg_slice) = checked_tys.list(ty_args) else { continue };
        if let Some(&err_ty) = ty_arg_slice.get(1) {
            if !matches!(checked_tys.node(err_ty), Ok(PNode::Infer | PNode::Error)) {
                let mut label = OwnString::from_str_in("? ", alloc);
                format_ty_in(checked_tys, err_ty, cx, &mut label, alloc);
                push_hint(&mut hints, item_byte_start + end, HintKind::TryPropagate, label);
            }
        } else if ty_arg_slice.len() == 1 {
            let label = OwnString::from_str_in("? None", alloc);
            push_hint(&mut hints, item_byte_start + end, HintKind::TryPropagate, label);
        }
    }

    // 3. Binding hints: locals, match arms (including narrowed union residuals), if-let, closure params.
    if opts.bindings {
        let mut skip_pat: OwnVec<bool, A> = OwnVec::with_capacity_in(body_view.pats.len(), alloc);
        skip_pat.resize(body_view.pats.len(), false);

        // Skip fn parameters (always explicitly typed in Rust signatures).
        for &p in body_view.params {
            mark_pat_tree(body_view, p, &mut skip_pat, alloc);
        }
        // Skip `let pat: ConcreteTy = ...` when the annotation has no `_` hole.
        for stmt in body_view.stmts {
            if let Stmt::Let { pat, ty: Some(ann_ty), .. } = *stmt
                && !has_infer_hole(body_view.types, ann_ty, alloc)
            {
                mark_pat_tree(body_view, pat, &mut skip_pat, alloc);
            }
        }
        // Skip explicitly typed closure parameters.
        for expr in body_view.exprs {
            if let Expr::Closure { params, param_tys, .. } = *expr
                && let Some(p_slice) = body_view.pat_lists.get(params.range())
                && let Some(t_slice) = body_view.opt_types.get(param_tys.range())
            {
                for (&p, &opt_ty) in p_slice.iter().zip(t_slice.iter()) {
                    if let Some(ann_ty) = opt_ty
                        && !has_infer_hole(body_view.types, ann_ty, alloc)
                    {
                        mark_pat_tree(body_view, p, &mut skip_pat, alloc);
                    }
                }
            }
        }

        for (idx, pat) in body_view.pats.iter().enumerate() {
            if skip_pat.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let Pat::Bind { name, sub, ty: written_ty, .. } = *pat else { continue };
            // Skip typed union patterns like `a: ErrA => ...`.
            if let Some(wt) = written_ty
                && !has_infer_hole(body_view.types, wt, alloc)
            {
                continue;
            }
            let Some(ident) = cx.spell(name) else { continue };
            if ident == "_" {
                continue;
            }
            let Some(range) = body_view.pat_ranges.get(idx).copied() else { continue };
            let (Ok(start), Ok(end)) = (usize::try_from(range.start), usize::try_from(range.end)) else {
                continue;
            };
            let Some(pat_text) = item_text.get(start..end) else { continue };
            let Some(rel_end) = binding_hint_offset(pat_text, start, ident, sub.is_some()) else {
                continue;
            };
            let local_ty = checked
                .local_ty
                .get(idx)
                .copied()
                .filter(|&t| !matches!(checked_tys.node(t), Ok(PNode::Error | PNode::Infer)))
                .or_else(|| checked.pat_ty.get(idx).copied());
            let Some(ty) = local_ty else { continue };
            if matches!(checked_tys.node(ty), Ok(PNode::Error | PNode::Infer)) {
                continue;
            }
            let mut label = OwnString::from_str_in(": ", alloc);
            format_ty_in(checked_tys, ty, cx, &mut label, alloc);
            push_hint(&mut hints, item_byte_start + rel_end, HintKind::Binding, label);
        }
    }

    // 4. Implicitly defaulted fields on struct & structural record literals.
    if opts.defaults {
        for (idx, expr) in body_view.exprs.iter().enumerate() {
            if !matches!(expr, Expr::Record { .. } | Expr::Struct { .. }) {
                continue;
            }
            let Ok(eid_u32) = u32::try_from(idx) else { continue };
            let eid = ExprId(eid_u32);
            let (def_indices, _) = checked.default_fields_of(eid);
            if def_indices.is_empty() {
                continue;
            }
            let Some(range) = body_view.expr_ranges.get(idx).copied() else { continue };
            let (Ok(start), Ok(end)) = (usize::try_from(range.start), usize::try_from(range.end)) else {
                continue;
            };
            let Some(expr_slice) = item_text.get(start..end) else { continue };
            let Some(close_rel) = expr_slice.rfind('}') else { continue };

            let (fru_indices, fru_tys) = checked.fru_fields_of(eid);
            let expr_ty = checked.expr_ty.get(idx).copied();

            let mut label = OwnString::from_str_in("..defaults: ", alloc);
            for (pos, &field_idx) in def_indices.iter().enumerate() {
                if pos > 0 {
                    label.push_str(", ");
                }
                let field_idx_usize = usize::try_from(field_idx).unwrap_or(usize::MAX);
                let field_ty = find_fru_ty(fru_indices, fru_tys, field_idx);
                write_default_field_entry(
                    checked,
                    checked_tys,
                    eid,
                    expr_ty,
                    field_idx_usize,
                    field_ty,
                    cx,
                    &mut label,
                    alloc,
                );
            }
            push_hint(&mut hints, item_byte_start + start + close_rel, HintKind::DefaultFields, label);
        }
    }

    // 5. Implicit coercions (`UnionInject`, `UnionWiden`, `RecordProject`, `Unsize`,
    //    `ClosureFnPointer`, nontrivial overloaded `Deref`).
    if opts.coercions {
        let adj_exprs = checked.adjusts.column::<0>();
        let adj_kinds = checked.adjusts.column::<1>();
        let adj_tys = checked.adjusts.column::<2>();
        let count = adj_exprs.len().min(adj_kinds.len()).min(adj_tys.len());
        for i in 0..count {
            let (Some(&eid), Some(&kind), Some(&target_ty)) =
                (adj_exprs.get(i), adj_kinds.get(i), adj_tys.get(i))
            else {
                continue;
            };
            if synthetic_expr.get(eid.ix()).copied().unwrap_or(false) {
                continue;
            }
            if matches!(checked_tys.node(target_ty), Ok(PNode::Error | PNode::Infer)) {
                continue;
            }
            let prefix = match kind {
                AdjustKind::UnionInject { .. } | AdjustKind::UnionWiden => "widen -> ",
                AdjustKind::RecordProject => "project -> ",
                AdjustKind::Unsize => "unsize -> ",
                AdjustKind::ClosureFnPointer => "fn ptr -> ",
                AdjustKind::Deref { overloaded: Some(_) } => {
                    // Only show overloaded deref coercions when target type differs from
                    // the expression's inner referenced type (e.g. `&String -> str`).
                    let src_ty = checked.expr_ty.get(eid.ix()).copied();
                    let redundant = src_ty.is_some_and(|st| match checked_tys.node(st) {
                        Ok(PNode::Ref(_, inner)) => inner == target_ty,
                        _ => st == target_ty,
                    });
                    if redundant {
                        continue;
                    }
                    "deref -> "
                }
                AdjustKind::NeverToAny
                | AdjustKind::Deref { overloaded: None }
                | AdjustKind::Borrow { .. }
                | AdjustKind::ReifyFnPointer
                | AdjustKind::UnsafeFnPointer
                | AdjustKind::MutToConstPointer => continue,
            };
            let Some(range) = body_view.expr_ranges.get(eid.ix()).copied() else { continue };
            let (Ok(start), Ok(end)) = (usize::try_from(range.start), usize::try_from(range.end)) else {
                continue;
            };
            if end <= start || end > item_text.len() {
                continue;
            }
            let mut label = OwnString::from_str_in(prefix, alloc);
            format_ty_in(checked_tys, target_ty, cx, &mut label, alloc);
            push_hint(&mut hints, item_byte_start + end, HintKind::Coercion, label);
        }
    }

    // 6. Optional expression type hints (`--all-exprs`).
    if opts.all_exprs {
        for (idx, expr) in body_view.exprs.iter().enumerate() {
            if synthetic_expr.get(idx).copied().unwrap_or(false) {
                continue;
            }
            if matches!(
                expr,
                Expr::Missing
                    | Expr::Block { .. }
                    | Expr::Return { .. }
                    | Expr::Break { .. }
                    | Expr::Continue { .. }
                    | Expr::Assign { .. }
                    | Expr::AssignOp { .. }
                    | Expr::Underscore
            ) {
                continue;
            }
            let Some(range) = body_view.expr_ranges.get(idx).copied() else { continue };
            let (Ok(start), Ok(end)) = (usize::try_from(range.start), usize::try_from(range.end)) else {
                continue;
            };
            if end <= start || end > item_text.len() {
                continue;
            }
            let Some(&ty) = checked.expr_ty.get(idx) else { continue };
            if matches!(checked_tys.node(ty), Ok(PNode::Error | PNode::Infer)) {
                continue;
            }
            let mut label = OwnString::from_str_in(": ", alloc);
            format_ty_in(checked_tys, ty, cx, &mut label, alloc);
            push_hint(&mut hints, item_byte_start + end, HintKind::ExprType, label);
        }
    }

    // Deterministic sort by `(byte_offset, kind, label)` and dedup.
    hints.sort_by(|a, b| {
        a.byte_offset
            .cmp(&b.byte_offset)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.label.as_str().cmp(b.label.as_str()))
    });
    dedup_hints(&mut hints);
    hints
}

fn mark_expr_shallow_synthetic<A: Allocator + Copy>(
    body: &BodyView<'_>,
    root: ExprId,
    marked: &mut [bool],
    alloc: A,
) {
    let mut stack: OwnVec<ExprId, A> = OwnVec::with_capacity_in(8, alloc);
    stack.push(root);
    let mut steps = 0usize;
    while let Some(eid) = stack.pop() {
        steps += 1;
        if steps > 64 {
            break;
        }
        let Some(slot) = marked.get_mut(eid.ix()) else { continue };
        if *slot {
            continue;
        }
        *slot = true;
        let Some(&expr) = body.exprs.get(eid.ix()) else { continue };
        match expr {
            Expr::Return { value: Some(v) } | Expr::Break { value: Some(v), .. } | Expr::Paren(v) => {
                stack.push(v);
            }
            Expr::Call { callee, args } => {
                stack.push(callee);
                if let Some(slice) = body.expr_lists.get(args.range()) {
                    for &a in slice {
                        stack.push(a);
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_result_error_hole<A: Allocator + Copy>(store: &Store<'_>, owner: ItemKey, alloc: A) -> bool {
    let Some(head) = store.fns.get(&owner) else { return false };
    let tys = store.tys.view();
    let Ok(PNode::Adt(_, args)) = tys.node(head.output) else { return false };
    let Ok(slice) = tys.list(args) else { return false };
    let (Some(&ok_ty), Some(&err_ty)) = (slice.first(), slice.get(1)) else { return false };
    slice.len() == 2 && !has_infer_hole(tys, ok_ty, alloc) && has_infer_hole(tys, err_ty, alloc)
}

fn find_fru_ty(fru_indices: &[u32], fru_tys: &[PTy], wanted: u32) -> Option<PTy> {
    for (i, &idx) in fru_indices.iter().enumerate() {
        if idx == wanted {
            return fru_tys.get(i).copied();
        }
    }
    None
}

#[expect(clippy::too_many_arguments, reason = "formats one defaulted field from checked record or ADT metadata")]
fn write_default_field_entry<'a, 'n, S, A>(
    checked: &Checked<'_>,
    checked_tys: PTys<'_>,
    eid: ExprId,
    expr_ty: Option<PTy>,
    field_idx: usize,
    fru_ty: Option<PTy>,
    cx: &TypeFmtCx<'a, 'n, S, A>,
    out: &mut OwnString<A>,
    alloc: A,
) where
    S: NamesSource<'n>,
    A: Allocator + Copy,
{
    let mut name_written = false;
    let mut fallback_ty: Option<(PTys<'_>, PTy)> = None;

    if let Some(et) = expr_ty {
        match checked_tys.node(et) {
            Ok(PNode::Record(schema, fields_list)) => {
                if let Some(names) = checked_tys.record_names(schema)
                    && let Some(&nm) = names.get(field_idx)
                    && let Some(spelled) = cx.spell(nm)
                {
                    out.push_str(spelled);
                    name_written = true;
                }
                if let Ok(list) = checked_tys.list(fields_list)
                    && let Some(&ty) = list.get(field_idx)
                {
                    fallback_ty = Some((checked_tys, ty));
                }
            }
            Ok(PNode::Adt(adt_key, _)) => {
                if let Some(adt) = cx.store.adts.get(&adt_key) {
                    let variant_idx = find_expr_variant(checked, eid).unwrap_or(0);
                    if let Some(variant) = adt.variants.get(variant_idx)
                        && let Some(fields) = adt.fields.get(variant.fields.range())
                        && let Some(field) = fields.get(field_idx)
                    {
                        if let Some(spelled) = cx.spell(field.name) {
                            out.push_str(spelled);
                            name_written = true;
                        }
                        fallback_ty = Some((cx.store.tys.view(), field.ty));
                    }
                }
            }
            _ => {}
        }
    }

    if !name_written {
        let _ = write!(out, "_{field_idx}");
    }

    if let Some(ty) = fru_ty {
        if !matches!(checked_tys.node(ty), Ok(PNode::Error | PNode::Infer)) {
            out.push_str(": ");
            format_ty_in(checked_tys, ty, cx, out, alloc);
            return;
        }
    }
    if let Some((tys_view, ty)) = fallback_ty
        && !matches!(tys_view.node(ty), Ok(PNode::Error | PNode::Infer))
    {
        out.push_str(": ");
        format_ty_in(tys_view, ty, cx, out, alloc);
    }
}

fn find_expr_variant(checked: &Checked<'_>, eid: ExprId) -> Option<usize> {
    let exprs = checked.expr_defs.column::<0>();
    let defs = checked.expr_defs.column::<1>();
    let at = exprs.binary_search(&eid).ok()?;
    match defs.get(at)? {
        tc::Def::Ctor { variant, .. } => usize::try_from(*variant).ok(),
        tc::Def::Adt(_) => Some(0),
        _ => None,
    }
}

fn dedup_hints<A: Allocator>(hints: &mut OwnVec<Hint<A>, A>) {
    let mut write_at = 0usize;
    for read_at in 0..hints.len() {
        let is_dup = write_at > 0 && {
            let prev = hints.get(write_at - 1);
            let cur = hints.get(read_at);
            match (prev, cur) {
                (Some(p), Some(c)) => {
                    p.byte_offset == c.byte_offset
                        && p.kind == c.kind
                        && p.label.as_str() == c.label.as_str()
                }
                _ => false,
            }
        };
        if !is_dup {
            if write_at != read_at {
                hints.swap(write_at, read_at);
            }
            write_at += 1;
        }
    }
    hints.truncate(write_at);
}

/// Builds the compact resolved signature line `fn name(p: T) -> Ret` for an inspected item.
fn format_resolved_signature<'a, 'n, S, A>(
    fn_name: &str,
    item_text: &str,
    owner: ItemKey,
    body_view: &BodyView<'_>,
    cx: &TypeFmtCx<'a, 'n, S, A>,
    alloc: A,
) -> OwnString<A>
where
    S: NamesSource<'n>,
    A: Allocator + Copy,
{
    let mut sig = OwnString::new_in(alloc);
    let trimmed = item_text.trim_start();
    // Preserve leading visibility / qualifiers succinctly if present before `fn`.
    let head_prefix = item_text
        .get(..find_body_brace(item_text))
        .unwrap_or(trimmed);
    if has_leading_keyword(head_prefix, "pub") {
        sig.push_str("pub ");
    }
    if has_leading_keyword(head_prefix, "const") {
        sig.push_str("const ");
    }
    if let Some(head) = cx.store.fns.get(&owner)
        && head.header.safety == Safety::Unsafe
    {
        sig.push_str("unsafe ");
    }
    sig.push_str("fn ");
    sig.push_str(fn_name);
    sig.push('(');

    let store_tys = cx.store.tys.view();
    if let Some(head) = cx.store.fns.get(&owner) {
        for (i, &input_ty) in head.inputs.iter().enumerate() {
            if i > 0 {
                sig.push_str(", ");
            }
            let param_written = body_view
                .params
                .get(i)
                .and_then(|&pid| body_view.pat_ranges.get(pid.ix()).copied())
                .and_then(|r| {
                    let s = usize::try_from(r.start).ok()?;
                    let e = usize::try_from(r.end).ok()?;
                    item_text.get(s..e)
                })
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match param_written {
                Some(p) => sig.push_str(p),
                None => {
                    let _ = write!(sig, "arg{i}");
                }
            }
            sig.push_str(": ");
            format_ty_in(store_tys, input_ty, cx, &mut sig, alloc);
        }
        sig.push(')');
        if let Some(out_ty) = cx.store.fn_output(owner) {
            let is_unit = matches!(store_tys.node(out_ty), Ok(PNode::Tuple(l)) if l.len == 0);
            if !is_unit {
                sig.push_str(" -> ");
                format_ty_in(store_tys, out_ty, cx, &mut sig, alloc);
            }
        }
    } else {
        sig.push(')');
    }
    sig
}

fn has_leading_keyword(header: &str, kw: &str) -> bool {
    let mut rest = header;
    // Skip leading `#[...]` attributes iteratively.
    let mut guard = 0usize;
    while guard < 32 {
        guard += 1;
        rest = rest.trim_start();
        if rest.starts_with("#[") {
            let bytes = rest.as_bytes();
            let mut depth = 0u32;
            let mut i = 1usize;
            while i < bytes.len() {
                match bytes.get(i) {
                    Some(b'[') => depth += 1,
                    Some(b']') => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            rest = rest.get(i + 1..).unwrap_or("");
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            continue;
        }
        break;
    }
    rest.strip_prefix(kw)
        .is_some_and(|after| after.starts_with(|c: char| c.is_ascii_whitespace() || c == '('))
}

/// Single in-memory source file provider for `blaze_scope::resolve::resolve_crates`.
struct OneSource<'a> {
    text: &'a str,
    env: CrateEnv<'a>,
}

impl<'a> SourceTexts<'a> for OneSource<'a> {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        (krate == 0 && path == INSPECT_ROOT).then_some(self.text)
    }

    fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'a>> {
        (krate == 0).then_some(&self.env)
    }
}

/// Candidate function or method item discovered in crate 0.
enum FnCandidate<'n> {
    Free {
        lib: LibKey,
        name: &'n str,
        text: &'n str,
        byte_start: usize,
    },
    Member {
        container_lib: LibKey,
        container_name: &'n str,
        member_name: &'n str,
        text: &'n str,
        byte_start: usize,
    },
}

impl<'n> FnCandidate<'n> {
    fn byte_start(&self) -> usize {
        match self {
            FnCandidate::Free { byte_start, .. } | FnCandidate::Member { byte_start, .. } => *byte_start,
        }
    }

    fn text(&self) -> &'n str {
        match self {
            FnCandidate::Free { text, .. } | FnCandidate::Member { text, .. } => text,
        }
    }

    fn matches_filter(&self, filter: Option<&str>) -> bool {
        let Some(wanted) = filter else { return true };
        match self {
            FnCandidate::Free { name, .. } => *name == wanted,
            FnCandidate::Member { container_name, member_name, .. } => {
                if *member_name == wanted {
                    return true;
                }
                if let Some((lhs, rhs)) = wanted.rsplit_once("::") {
                    return rhs == *member_name && (*container_name == lhs || container_name.ends_with(lhs));
                }
                false
            }
        }
    }
}

const fn to_surface_edition(ed: RustEdition) -> SurfaceEdition {
    match ed {
        RustEdition::Edition2015 => SurfaceEdition::E2015,
        RustEdition::Edition2018 => SurfaceEdition::E2018,
        RustEdition::Edition2021 => SurfaceEdition::E2021,
        RustEdition::Edition2024 => SurfaceEdition::E2024,
    }
}

fn implicit_for(edition: PackageEdition) -> Implicit {
    match edition.blaze {
        Some(blaze) => Implicit {
            std: StdIs::Bstd(blaze),
            prelude: Prelude::Blaze(blaze),
            proc_macro: false,
            entry: false,
        },
        None => Implicit {
            std: StdIs::Rust,
            prelude: Prelude::Rust(edition.syntax),
            proc_macro: false,
            entry: false,
        },
    }
}

/// Extracts the short type name from an `impl` header (e.g. `impl Opts` -> `"Opts"`).
fn short_impl_target(header: &str) -> &str {
    let trimmed = header.trim();
    let after_for = match trimmed.rsplit_once(" for ") {
        Some((_, rhs)) => rhs.trim(),
        None => trimmed.strip_prefix("impl").map_or(trimmed, str::trim),
    };
    let head = after_for.split('{').next().unwrap_or(after_for).trim();
    let without_where = head.split("where").next().unwrap_or(head).trim();
    // Skip leading generic list `<...>` on `impl<T> Foo<T>`.
    let without_generics = if without_where.starts_with('<') {
        let bytes = without_where.as_bytes();
        let mut depth = 0u32;
        let mut cut = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                b'<' => depth += 1,
                b'>' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        cut = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        without_where.get(cut..).unwrap_or(without_where).trim()
    } else {
        without_where
    };
    without_generics.split('<').next().unwrap_or(without_generics).trim()
}

/// Inspects a single Blaze or Rust source file and returns structured inlay hints for every
/// matching function or method.
///
/// Uses `front2` directly with zero filesystem I/O. Pass `store_alloc` (e.g. `heap.alloc()`) for
/// `Store`'s internal tables and `alloc` for the returned [`InspectOutput`].
pub fn inspect_source<A: Allocator + Copy>(
    source: &str,
    edition: PackageEdition,
    filter_item: Option<&str>,
    opts: InspectOpts,
    store_alloc: Alloc<'static>,
    alloc: A,
) -> Result<InspectOutput<A>, InspectError> {
    let names_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).map_err(InspectError::Frame)?;
    let names_erased = blaze_own::Erased::new(&names_frame);
    let bump = names_erased.bump();

    let facts = EnvFacts {
        crate_name: "inspect",
        package: Some(("inspect", "0.0.0")),
        bin: None,
        seen: SeenRoot::Package,
        read_folder: "/inspect/",
        build_script: false,
        script: None,
        test_or_bench: false,
        bin_exes: BinNames::NONE,
        meta: None,
    };
    let crate_env = CrateEnv::of(facts, store_alloc).map_err(|_| InspectError::CrateEnv)?;
    let one_source = OneSource { text: source, env: crate_env };
    let target_cfg = Cfg::of(&Target::AARCH64_APPLE_DARWIN, &[], bump.alloc());
    let input = CrateInput {
        name: "inspect",
        key: CrateItemKey(ContentId::of("blaze-ladder front2 inspect crate", b"inspect")),
        edition: to_surface_edition(edition.syntax),
        implicit: implicit_for(edition),
        root: INSPECT_ROOT,
        shown_from: "/inspect/",
        features: &[],
        deps: &[],
        cfg: target_cfg.atoms(),
        script: None,
    };

    let names = resolve_crates(&[input], Sysroot::default(), &one_source, &Serial, bump).map_err(|e| {
        let mut message = OwnString::new_in(store_alloc);
        let _ = write!(message, "{e}");
        InspectError::Resolve { message }
    })?;

    let front_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).map_err(InspectError::Frame)?;
    let source_files = [SourceFile { path: "src/lib.rs", text: source }];
    let mut front = Front2::new(&names, &front_frame, &source_files, store_alloc).map_err(InspectError::Front)?;
    let prepared = Prepares::new_in(bump);
    let mut store = Store::new(front.index.lang("sized"), front.frame, store_alloc);
    let (mut buckets, mut consts) = (None, Consts::default());

    let name_table = NameTable::from_source(source, alloc);
    let line_index = LineIndex::new(source, alloc);

    // Collect every function and impl method in crate 0, sorted by source offset.
    let mut candidates: OwnVec<FnCandidate<'_>, A> = OwnVec::new_in(alloc);
    for (lib_key, _, _) in names.items(0) {
        let Some(kind) = NamesSource::kind_of_item(&names, lib_key) else { continue };
        match kind {
            ItemKind::Function => {
                let Some(it) = NamesSource::text_of(&names, lib_key) else { continue };
                let Some(name) = it.name else { continue };
                let byte_start = match NamesSource::item_start(&names, lib_key) {
                    Some((At::File(0), off)) => usize::try_from(off).unwrap_or(0),
                    _ => source.find(it.text).unwrap_or(0),
                };
                candidates.push(FnCandidate::Free {
                    lib: lib_key,
                    name,
                    text: it.text,
                    byte_start,
                });
            }
            ItemKind::Impl => {
                let Some(impl_it) = NamesSource::text_of(&names, lib_key) else { continue };
                let container_start = match NamesSource::item_start(&names, lib_key) {
                    Some((At::File(0), off)) => usize::try_from(off).unwrap_or(0),
                    _ => source.find(impl_it.text).unwrap_or(0),
                };
                let container_name = impl_it.impl_header.map_or("impl", short_impl_target);
                let mut search_from = 0usize;
                NamesSource::each_member(&names, lib_key, &mut |member| {
                    if member.kind != ItemKind::Function {
                        return;
                    }
                    let Some(m_name) = member.name else { return };
                    let rel = impl_it
                        .text
                        .get(search_from..)
                        .and_then(|tail| tail.find(member.text))
                        .map(|off| search_from + off)
                        .or_else(|| impl_it.text.find(member.text))
                        .unwrap_or(0);
                    search_from = rel.saturating_add(member.text.len());
                    candidates.push(FnCandidate::Member {
                        container_lib: lib_key,
                        container_name,
                        member_name: m_name,
                        text: member.text,
                        byte_start: container_start.saturating_add(rel),
                    });
                });
            }
            _ => {}
        }
    }

    candidates.sort_by_key(FnCandidate::byte_start);

    let mut items: OwnVec<InspectedItem<A>, A> = OwnVec::new_in(alloc);
    let mut matched_any = false;
    let mut first_failure: Option<OwnString<Alloc<'static>>> = None;

    for cand in candidates.iter() {
        if !cand.matches_filter(filter_item) {
            continue;
        }
        matched_any = true;

        let item_text = cand.text();
        let byte_start = cand.byte_start();
        let byte_end = (byte_start + item_text.len()).min(source.len());

        let lowered_res = match cand {
            FnCandidate::Free { lib, .. } => {
                lower_with_consts(&names, prepared, &mut front, &mut store, &mut buckets, &mut consts, |k, s| {
                    lower_fn(&names, prepared, k, s, *lib)
                })
            }
            FnCandidate::Member { container_lib, member_name, .. } => {
                let container_ck = checker_of(*container_lib);
                match front.keys.member(container_ck, MemberKind::Fn, member_name) {
                    Ok(member_ck) => {
                        lower_with_consts(&names, prepared, &mut front, &mut store, &mut buckets, &mut consts, |k, s| {
                            lower_member(&names, prepared, k, s, member_ck)
                        })
                    }
                    Err(e) => {
                        if first_failure.is_none() {
                            let mut msg = OwnString::new_in(store_alloc);
                            let _ = write!(msg, "key error: {e}");
                            first_failure = Some(msg);
                        }
                        continue;
                    }
                }
            }
        };

        let mut body = match lowered_res {
            Ok(b) => b,
            Err(LowerStepError::Body(BodyError::NoBody(_))) => continue,
            Err(e) => {
                if first_failure.is_none() {
                    let mut msg = OwnString::new_in(store_alloc);
                    let _ = write!(msg, "lowering failed: {e}");
                    first_failure = Some(msg);
                }
                continue;
            }
        };

        let checked = {
            let mut records = Records {
                names: &names,
                keys: &mut front.keys,
                index: &front.index,
                store: &mut store,
                buckets: &mut buckets,
                order: BuildOrder::AsAsked,
                consts: Some((&mut consts, prepared)),
            };
            let res = if body.typeck.inline_fors.is_empty() {
                let view = body.lowered.view();
                let Ok(spell) = Spelling::of(&view) else { continue };
                let scope = body.scope(&spell);
                check_body(&mut records, &body, &scope)
            } else {
                super::inline_for::unroll_concrete_body(&mut records, prepared, &mut body)
            };
            match res {
                Ok(done) => done,
                Err(err) => {
                    if first_failure.is_none() {
                        let mut msg = OwnString::new_in(store_alloc);
                        let _ = write!(msg, "{err}");
                        first_failure = Some(msg);
                    }
                    continue;
                }
            }
        };
        let view = body.lowered.view();
        let Ok(spell) = Spelling::of(&view) else { continue };

        let Ok(body_tys) = body.tys.view(&store.tys) else { continue };
        let body_view = body.typeck.view(body_tys);
        let cx = TypeFmtCx {
            names: &names,
            keys: &front.keys,
            store: &store,
            name_table: &name_table,
            spelling: Some(&spell),
            generic_params: &body.params,
        };

        let mut display_name = OwnString::new_in(alloc);
        match cand {
            FnCandidate::Free { name, .. } => display_name.push_str(name),
            FnCandidate::Member { container_name, member_name, .. } => {
                display_name.push_str(container_name);
                display_name.push_str("::");
                display_name.push_str(member_name);
            }
        }

        let signature = format_resolved_signature(
            display_name.as_str(),
            item_text,
            body.owner,
            &body_view,
            &cx,
            alloc,
        );
        let hints = collect_body_hints(
            source,
            item_text,
            byte_start,
            body.owner,
            &body_view,
            &checked,
            &cx,
            opts,
            alloc,
        );

        let store_tys = store.tys.view();
        let inferred_output = store.inferred_return_type(body.owner).map(|ty| {
            let mut s = OwnString::new_in(alloc);
            format_ty_in(store_tys, ty, &cx, &mut s, alloc);
            s
        });
        let inferred_error_set = if is_result_error_hole(&store, body.owner, alloc) {
            store.inferred_error_set(body.owner).map(|ty| {
                let mut s = OwnString::new_in(alloc);
                format_ty_in(store_tys, ty, &cx, &mut s, alloc);
                s
            })
        } else {
            None
        };

        let (start_line, _) = line_index.line_col(source, byte_start);
        let end_probe = byte_end.saturating_sub(1).max(byte_start);
        let (end_line, _) = line_index.line_col(source, end_probe);

        items.push(InspectedItem {
            name: display_name,
            signature,
            inferred_output,
            inferred_error_set,
            line: start_line,
            end_line,
            byte_start,
            byte_end,
            hints,
        });
    }

    if filter_item.is_some() && !matched_any {
        return Err(InspectError::ItemNotFound);
    }
    if items.is_empty()
        && let Some(message) = first_failure
    {
        return Err(InspectError::CheckFailed { message });
    }

    Ok(InspectOutput {
        source: OwnString::from_str_in(source, alloc),
        items,
    })
}

enum LowerStepError {
    Body(BodyError),
    ConstEval(ConstFail),
}

impl fmt::Display for LowerStepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LowerStepError::Body(e) => write!(f, "{e}"),
            LowerStepError::ConstEval(e) => write!(f, "{e:?}"),
        }
    }
}

fn lower_with_consts<'n>(
    names: &'n Names<'n>,
    prepared: &Prepares<'n>,
    front: &mut Front2<'n>,
    store: &mut Store<'n>,
    buckets: &mut Option<super::buckets::Buckets>,
    consts: &mut Consts<'n>,
    mut lower: impl FnMut(&mut Keys, &mut Store<'n>) -> Result<Body<'n>, BodyError>,
) -> Result<Body<'n>, LowerStepError> {
    let mut lens = 0u32;
    loop {
        match lower(&mut front.keys, store) {
            Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. })
            | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. })
                if lens < MOST_LENS =>
            {
                lens += 1;
                let mut records = Records {
                    names,
                    keys: &mut front.keys,
                    index: &front.index,
                    store,
                    buckets,
                    order: BuildOrder::AsAsked,
                    consts: Some((consts, prepared)),
                };
                if let Err(err) = eval_len(&mut records, c) {
                    return Err(LowerStepError::ConstEval(err));
                }
            }
            Ok(b) => return Ok(b),
            Err(e) => return Err(LowerStepError::Body(e)),
        }
    }
}

/// Renders [`InspectOutput`] as annotated source with resolved signature headers, 1-based line
/// numbers, and inline `/* ... */` inlay hints spliced cleanly at UTF-8 character boundaries.
pub fn render_annotated<A: Allocator + Copy>(output: &InspectOutput<A>, alloc: A) -> OwnString<A> {
    let source = output.source.as_str();
    let lines = LineIndex::new(source, alloc);
    let mut out = OwnString::with_capacity_in(source.len().saturating_mul(2), alloc);

    // Compute line-number gutter width from the last inspected line.
    let max_line = output.items.iter().map(|it| it.end_line).max().unwrap_or(1);
    let gutter_width = decimal_digits(max_line).max(2);

    for (idx, item) in output.items.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        // Compact resolved signature header.
        out.push_str("// ");
        out.push_str(item.signature.as_str());
        out.push('\n');

        let start_line = usize::try_from(item.line.saturating_sub(1)).unwrap_or(0);
        let end_line_incl = usize::try_from(item.end_line.saturating_sub(1)).unwrap_or(start_line);
        let mut hint_cursor = 0usize;

        for line_idx in start_line..=end_line_incl {
            let Some(&line_start) = lines.starts.get(line_idx) else { break };
            let raw_end = lines
                .starts
                .get(line_idx + 1)
                .copied()
                .unwrap_or(source.len());
            let mut line_end = raw_end;
            if line_end > line_start && source.as_bytes().get(line_end - 1) == Some(&b'\n') {
                line_end -= 1;
            }
            if line_end > line_start && source.as_bytes().get(line_end - 1) == Some(&b'\r') {
                line_end -= 1;
            }

            let line_num = line_idx + 1;
            let _ = write!(out, "{line_num:>gutter_width$} | ");

            // Advance past any hints before this line.
            while let Some(h) = item.hints.get(hint_cursor) {
                if h.byte_offset < line_start {
                    hint_cursor += 1;
                } else {
                    break;
                }
            }

            let mut cursor = line_start;
            while let Some(h) = item.hints.get(hint_cursor) {
                if h.byte_offset > line_end {
                    break;
                }
                let at = clamp_char_boundary(source, h.byte_offset).max(cursor).min(line_end);
                if let Some(chunk) = source.get(cursor..at) {
                    out.push_str(chunk);
                }
                write_inline_hint(h, source, at, &mut out);
                cursor = at;
                hint_cursor += 1;
            }
            if let Some(tail) = source.get(cursor..line_end) {
                out.push_str(tail);
            }
            out.push('\n');
        }
    }
    out
}

fn write_inline_hint<A: Allocator>(hint: &Hint<A>, source: &str, at: usize, out: &mut OwnString<A>) {
    let prev_byte = at.checked_sub(1).and_then(|p| source.as_bytes().get(p).copied());
    let next_byte = source.as_bytes().get(at).copied();
    match hint.kind {
        HintKind::Binding | HintKind::ExprType => {
            out.push_str("/*");
            out.push_str(hint.label.as_str());
            out.push_str("*/");
        }
        HintKind::DefaultFields => {
            if prev_byte.is_some_and(|b| !b.is_ascii_whitespace() && b != b'{') {
                out.push(' ');
            }
            out.push_str("/* ");
            out.push_str(hint.label.as_str());
            out.push_str(" */");
            if next_byte == Some(b'}') {
                out.push(' ');
            }
        }
        HintKind::InferredReturn | HintKind::Coercion | HintKind::TryPropagate => {
            if prev_byte.is_some_and(|b| !b.is_ascii_whitespace()) {
                out.push(' ');
            }
            out.push_str("/* ");
            out.push_str(hint.label.as_str());
            out.push_str(" */");
        }
    }
}

fn decimal_digits(mut n: u32) -> usize {
    let mut d = 1usize;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Renders [`InspectOutput`] as deterministic, properly escaped JSON.
pub fn render_json<A: Allocator + Copy>(output: &InspectOutput<A>, alloc: A) -> OwnString<A> {
    let mut out = OwnString::with_capacity_in(512, alloc);
    out.push_str("{\n  \"items\": [");
    for (i, item) in output.items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("\n    {\n      \"name\": ");
        write_json_str(item.name.as_str(), &mut out);
        out.push_str(",\n      \"signature\": ");
        write_json_str(item.signature.as_str(), &mut out);
        out.push_str(",\n      \"inferred_output\": ");
        match &item.inferred_output {
            Some(s) => write_json_str(s.as_str(), &mut out),
            None => out.push_str("null"),
        }
        out.push_str(",\n      \"inferred_error_set\": ");
        match &item.inferred_error_set {
            Some(s) => write_json_str(s.as_str(), &mut out),
            None => out.push_str("null"),
        }
        let _ = write!(
            out,
            ",\n      \"line\": {},\n      \"end_line\": {},\n      \"hints\": [",
            item.line, item.end_line
        );
        for (j, h) in item.hints.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "\n        {{\"line\": {}, \"col\": {}, \"byte_offset\": {}, \"kind\": ",
                h.line, h.col, h.byte_offset
            );
            write_json_str(h.kind.as_str(), &mut out);
            out.push_str(", \"label\": ");
            write_json_str(h.label.as_str(), &mut out);
            out.push('}');
        }
        if !item.hints.is_empty() {
            out.push_str("\n      ");
        }
        out.push_str("]\n    }");
    }
    if !output.items.is_empty() {
        out.push_str("\n  ");
    }
    out.push_str("]\n}\n");
    out
}

fn write_json_str<A: Allocator>(s: &str, out: &mut OwnString<A>) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLAZE_PRELUDE: &str = "#![blaze::records]
#![blaze::error_unions]
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for i32 {}
impl Copy for bool {}

enum Infallible {}

enum ControlFlow<B, C> {
    #[lang = \"Continue\"]
    Continue(C),
    #[lang = \"Break\"]
    Break(B),
}

#[lang = \"from\"]
trait From<T> {
    fn from(value: T) -> Self;
}

impl<T> From<T> for T {
    fn from(value: T) -> Self {
        value
    }
}

trait FromResidual<R = <Self as Try>::Residual> {
    #[lang = \"from_residual\"]
    fn from_residual(residual: R) -> Self;
}

trait Try: FromResidual {
    type Output;
    type Residual;
    #[lang = \"branch\"]
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output>;
}

enum Result<T, E> {
    Ok(T),
    Err(E),
}

impl<T, E> Try for Result<T, E> {
    type Output = T;
    type Residual = Result<Infallible, E>;
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output> {
        match self {
            Result::Ok(v) => ControlFlow::Continue(v),
            Result::Err(e) => ControlFlow::Break(Result::Err(e)),
        }
    }
}

impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F> {
    fn from_residual(residual: Result<Infallible, E>) -> Self {
        match residual {
            Result::Err(e) => Result::Err(From::from(e)),
            Result::Ok(x) => match x {},
        }
    }
}
";

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    #[test]
    fn inspect_inferred_error_union_try_and_residual_narrowing() {
        let alloc = test_alloc();
        let mut src = OwnString::from_str_in(BLAZE_PRELUDE, alloc);
        src.push_str(
            "
struct ErrA(u32);
struct ErrB(u32);
struct ErrC(u32);

fn read(mode: u32) -> Result<u32, _> {
    if mode == 1 {
        Result::Err(ErrA(10))
    } else {
        Result::Ok(mode)
    }
}

fn parse(x: u32) -> Result<u32, _> {
    if x == 2 {
        Result::Err(ErrB(20))
    } else {
        Result::Ok(x + 1)
    }
}

pub fn load(mode: u32) -> Result<u32, _> {
    if mode == 3 {
        return Result::Err(ErrC(30));
    }
    let a = read(mode)?;
    let b = parse(a)?;
    Result::Ok(b)
}

fn dispatch(mode: u32) -> u32 {
    match load(mode) {
        Result::Ok(v) => v,
        Result::Err(e) => match e {
            a: ErrA => a.0 + 1,
            rest => match rest {
                b: ErrB => b.0 + 2,
                c => c.0 + 3,
            },
        },
    }
}
",
        );

        let out = inspect_source(
            src.as_str(),
            BLAZE_EDITION,
            Some("load"),
            InspectOpts::default(),
            alloc,
            alloc,
        )
        .expect("inspect load");
        assert_eq!(out.items.len(), 1);
        let load = out.items.first().expect("load item");
        assert_eq!(load.name.as_str(), "load");
        let sig = load.signature.as_str();
        assert!(sig.contains("pub fn load(mode: u32) -> Result<u32, "), "sig: {sig}");
        assert!(sig.contains("ErrA") && sig.contains("ErrB") && sig.contains("ErrC"), "sig: {sig}");
        let err_set = load.inferred_error_set.as_ref().expect("inferred_error_set");
        assert!(
            err_set.as_str().contains("ErrA")
                && err_set.as_str().contains("ErrB")
                && err_set.as_str().contains("ErrC"),
            "err_set: {}",
            err_set.as_str()
        );
        assert!(
            load.hints.iter().any(|h| h.kind == HintKind::InferredReturn),
            "expected InferredReturn hint: {:?}",
            load.hints
        );
        assert!(
            load.hints.iter().any(|h| h.kind == HintKind::TryPropagate && h.label.as_str().contains("ErrA")),
            "expected TryPropagate ErrA hint: {:?}",
            load.hints
        );
        assert!(
            load.hints.iter().any(|h| h.kind == HintKind::Binding && h.label.as_str() == ": u32"),
            "expected Binding u32 hint: {:?}",
            load.hints
        );

        let annotated = render_annotated(&out, alloc);
        assert!(annotated.as_str().contains("/* = "));
        assert!(annotated.as_str().contains("/*: u32*/"));
        assert!(annotated.as_str().contains("/* ? ErrA */"));

        let json = render_json(&out, alloc);
        assert!(json.as_str().contains("\"name\": \"load\""));
        assert!(json.as_str().contains("\"inferred_error_set\":"));
        assert!(json.as_str().contains("\"kind\": \"try_propagate\""));

        // Inspect `dispatch` to verify narrowed residual binding types (`rest` -> 2-member union, `c` -> `ErrC`).
        let disp_out = inspect_source(
            src.as_str(),
            BLAZE_EDITION,
            Some("dispatch"),
            InspectOpts::default(),
            alloc,
            alloc,
        )
        .expect("inspect dispatch");
        let disp = disp_out.items.first().expect("dispatch item");
        assert!(
            disp.hints.iter().any(|h| h.kind == HintKind::Binding && h.label.as_str().contains("ErrB") && h.label.as_str().contains("ErrC")),
            "expected narrowed residual 2-member union binding for `rest`: {:?}",
            disp.hints
        );
        assert!(
            disp.hints.iter().any(|h| h.kind == HintKind::Binding && h.label.as_str() == ": ErrC"),
            "expected narrowed single-member binding for `c`: {:?}",
            disp.hints
        );
    }

    #[test]
    fn inspect_record_defaults_and_width_subtyping() {
        let alloc = test_alloc();
        let mut src = OwnString::from_str_in(BLAZE_PRELUDE, alloc);
        src.push_str(
            "
fn sum_def(r: { a: u32, b: u32 = 32 }) -> u32 {
    r.a + r.b
}

fn read_host(r: { host: u32 }) -> u32 {
    r.host
}

fn make_rec() -> _ {
    { a: 10u32, b: 20u32 }
}

fn demo() -> u32 {
    let x = sum_def({ a: 10u32 });
    let wide = { host: 20u32, port: 8080u32, tls: true };
    let y = read_host(wide);
    let r = make_rec();
    x + y + r.a
}
",
        );

        let out = inspect_source(
            src.as_str(),
            BLAZE_EDITION,
            Some("demo"),
            InspectOpts::default(),
            alloc,
            alloc,
        )
        .expect("inspect demo");
        let demo = out.items.first().expect("demo item");
        assert!(
            demo.hints.iter().any(|h| h.kind == HintKind::DefaultFields && h.label.as_str().contains("b: u32")),
            "expected DefaultFields b: u32 hint: {:?}",
            demo.hints
        );
        assert!(
            demo.hints.iter().any(|h| h.kind == HintKind::Coercion && h.label.as_str().contains("project -> { host: u32 }")),
            "expected RecordProject coercion hint: {:?}",
            demo.hints
        );
        assert!(
            demo.hints.iter().any(|h| h.kind == HintKind::Binding && h.label.as_str().contains("{ a: u32, b: u32 }")),
            "expected record binding hint for `r`: {:?}",
            demo.hints
        );

        let rec_out = inspect_source(
            src.as_str(),
            BLAZE_EDITION,
            Some("make_rec"),
            InspectOpts::default(),
            alloc,
            alloc,
        )
        .expect("inspect make_rec");
        let rec = rec_out.items.first().expect("make_rec item");
        assert_eq!(
            rec.inferred_output.as_ref().map(OwnString::as_str),
            Some("{ a: u32, b: u32 }")
        );
        assert_eq!(rec.signature.as_str(), "fn make_rec() -> { a: u32, b: u32 }");
    }

    #[test]
    fn inspect_plain_rust_without_no_core() {
        let alloc = test_alloc();
        let src = "
fn double(x: u32) -> _ {
    let y = x + x;
    let z: u32 = y;
    z
}
";
        let out = inspect_source(src, RUST_2024, None, InspectOpts::default(), alloc, alloc)
            .expect("inspect plain rust");
        assert_eq!(out.items.len(), 1);
        let f = out.items.first().expect("double");
        assert_eq!(f.signature.as_str(), "fn double(x: u32) -> u32");
        assert!(
            f.hints.iter().any(|h| h.kind == HintKind::Binding && h.label.as_str() == ": u32"),
            "expected binding hint for `y`: {:?}",
            f.hints
        );
        // `z` has an explicit `: u32` annotation, so only `y` should get a binding hint.
        let binding_count = f.hints.iter().filter(|h| h.kind == HintKind::Binding).count();
        assert_eq!(binding_count, 1, "explicitly typed `z` should not get a binding hint: {:?}", f.hints);
    }
}
