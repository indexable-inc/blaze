//! Item paths printed by `core::any::type_name` and the validity assertions: the ONE home for
//! both front2's constant evaluator (`ConstsWorld`) and the build seam (`Front2Program`,
//! `LazyProgram` -> `FrontAdts` -> `code_reads::Recorded`), rule 30.
//!
//! Matches rustc's def-path formatting:
//! - Module-level items by declaring module path from crate name (`core::option::Option`).
//! - Trait members as `crate::mod::Trait::member`.
//! - Inherent impl members on ADTs as `crate::mod::Type::member` (with `_` for generic
//!   parameters: `crate::mod::Gen<_>::member`), and on non-ADTs as `<SelfTy>::member`.
//! - Trait impl members as `<SelfTy as crate::mod::Trait<Args>>::member`.
//! - Body-local items (`fn`, `const`, `static`, `impl`) as `<enclosing>::name`, to any nesting
//!   depth (`crate::mod::outer::inner::deep`, `crate::Collider::spike::_f`,
//!   `<crate::Collider as crate::Tr>::tr_m::_tf`).
//! - Block-module items (`struct`, `enum`, `union`, `trait` declared in a body) under their
//!   owning body (`crate::mod::outer::LocalStruct`, `crate::Collider::spike::InSpike`), never
//!   leaking internal `__blaze_block_*` module names.
//!
//! Deterministic, non-recursive (one bounded work stack), and atomic: `out` is called once on
//! success and never called when returning `false`.

use core::fmt::Write as _;

use blaze_build::type_name::MAX_TEXT;
use blaze_own::{AVec, String as OwnString};
use blaze_surface::ItemKind;
use blaze_typeck::{FloatTy, IntTy, ItemKey, Mutbl, Name, PNode, PTy, PTys, UintTy};

use super::heads::Store;
use super::item_shown::write_impl_shown;
use super::keys::{Keys, MemberKind, Origin, checker_of};
use super::locals::LocalKind;
use super::source::NamesSource;

/// The most work-stack pops one item path may perform (rule 18: bounded, never recursive).
const MAX_STEPS: usize = 4096;

/// One piece on the iterative work stack.
#[derive(Clone, Copy)]
enum Piece<'a> {
    Path(ItemKey),
    Ty(PTy),
    Text(&'a str),
    /// A module path with internal `__blaze_block_*` segments omitted.
    ModPath { path: &'a str, trailing_sep: bool },
    /// Fallback impl header text formatted as a single-line `<...>` segment.
    ImplHeader(&'a str),
}

/// Writes `item`'s full path into `out` and returns `true`; returns `false` without calling
/// `out` when no path is known or the bound is exceeded.
pub(crate) fn item_path<'a, 'n: 'a, S: NamesSource<'n>>(
    names: S,
    keys: &'a Keys,
    store: &'a Store<'n>,
    item: ItemKey,
    out: &mut dyn FnMut(&str),
) -> bool {
    let alloc = store.tys.alloc();
    let tys = store.tys.view();
    let mut buf = OwnString::new_in(alloc);
    let mut stack: AVec<'_, Piece<'a>> = AVec::new_in(alloc);
    let mut seq: AVec<'_, Piece<'a>> = AVec::new_in(alloc);
    stack.push(Piece::Path(item));

    let mut steps = 0usize;
    while let Some(piece) = stack.pop() {
        steps += 1;
        if steps > MAX_STEPS {
            return false;
        }
        match piece {
            Piece::Text(t) => buf.push_str(t),
            Piece::ModPath { path, trailing_sep } => {
                let mut any = false;
                for seg in path.split("::").filter(|s| !s.is_empty() && !blaze_scope::blocks::is_block_module(s)) {
                    if any {
                        buf.push_str("::");
                    }
                    buf.push_str(seg);
                    any = true;
                }
                if any && trailing_sep {
                    buf.push_str("::");
                }
            }
            Piece::ImplHeader(header) => write_impl_shown(header, &mut buf),
            Piece::Path(at) => {
                if !expand_path(names, keys, store, tys, at, &mut stack) {
                    return false;
                }
            }
            Piece::Ty(t) => {
                seq.clear();
                if !expand_ty(names, keys, tys, t, &mut buf, &mut seq) {
                    return false;
                }
                for &p in seq.iter().rev() {
                    stack.push(p);
                }
            }
        }
        if buf.len() > MAX_TEXT {
            return false;
        }
    }
    if buf.is_empty() {
        return false;
    }
    out(buf.as_str());
    true
}

/// Pushes the pieces for `at` in reverse reading order onto `stack`.
fn expand_path<'a, 'n: 'a, S: NamesSource<'n>>(
    names: S,
    keys: &'a Keys,
    store: &'a Store<'n>,
    tys: PTys<'_>,
    at: ItemKey,
    stack: &mut AVec<'_, Piece<'a>>,
) -> bool {
    // 1. A member filed in `keys` (associated fn/const/type, struct ctor, anonymous const).
    if let Some(Origin::Member { container, kind, name }) = keys.origin(at) {
        match kind {
            MemberKind::Fn | MemberKind::Const | MemberKind::Type => {
                stack.push(Piece::Text(name));
                stack.push(Piece::Text("::"));
                stack.push(Piece::Path(*container));
                return true;
            }
            MemberKind::Ctor => {
                stack.push(Piece::Text("::{{constructor}}"));
                if !name.is_empty() {
                    stack.push(Piece::Text(name));
                    stack.push(Piece::Text("::"));
                }
                stack.push(Piece::Path(*container));
                return true;
            }
            MemberKind::Anon => {
                stack.push(Piece::Text("::{{constant}}"));
                stack.push(Piece::Path(*container));
                return true;
            }
        }
    }

    // 2. An impl whose resolved header is in `store.impls`. Checked before `store.locals` so a
    //    local impl formats from its resolved self type (which already carries the enclosing
    //    body path) rather than duplicating the enclosing body prefix.
    if let Some(imp) = store.impls.get(&at) {
        if let Some(trait_) = imp.trait_ {
            let Ok(trait_args) = tys.list(imp.trait_args.list()) else {
                return false;
            };
            stack.push(Piece::Text(">"));
            push_ty_args_rev(stack, trait_args);
            stack.push(Piece::Path(trait_));
            stack.push(Piece::Text(" as "));
            stack.push(Piece::Ty(imp.self_ty));
            stack.push(Piece::Text("<"));
        } else {
            let is_adt = matches!(tys.get(imp.self_ty), Some(PNode::Adt(..)));
            if !is_adt {
                stack.push(Piece::Text(">"));
            }
            stack.push(Piece::Ty(imp.self_ty));
            if !is_adt {
                stack.push(Piece::Text("<"));
            }
        }
        return true;
    }

    // 3. A body-local item (`fn`, `const`, `static`, or unfiled `impl` declared in a block).
    if let Some(local) = store.locals.head(at) {
        let declared = store.locals.body(local.enclosing).and_then(|b| b.rows.iter().find(|r| r.key == local.lib).map(|r| r.name));
        match declared {
            Some(name) if !name.is_empty() => {
                stack.push(Piece::Text(name));
                stack.push(Piece::Text("::"));
                stack.push(Piece::Path(local.enclosing));
            }
            _ if local.kind == LocalKind::Impl => {
                let Some(header) = local_impl_header(local.text) else {
                    return false;
                };
                stack.push(Piece::ImplHeader(header));
                stack.push(Piece::Text("::"));
                stack.push(Piece::Path(local.enclosing));
            }
            _ => {
                stack.push(Piece::Text("_"));
                stack.push(Piece::Text("::"));
                stack.push(Piece::Path(local.enclosing));
            }
        }
        return true;
    }

    // 4. A library item (module-level item, block-module item, unfiled member, variant, module).
    let Some(lib) = keys.library_in(names, at) else {
        return false;
    };
    if let Some((owner, name)) = names.member_owner(lib) {
        stack.push(Piece::Text(name));
        stack.push(Piece::Text("::"));
        stack.push(Piece::Path(checker_of(owner)));
        return true;
    }
    if let Some((enum_, index)) = names.variant_of(lib) {
        let Some(variant) = names.variants(enum_).and_then(|vs| usize::try_from(index).ok().and_then(|i| vs.get(i))) else {
            return false;
        };
        stack.push(Piece::Text("::{{constructor}}"));
        stack.push(Piece::Text(variant.name));
        stack.push(Piece::Text("::"));
        stack.push(Piece::Path(checker_of(enum_)));
        return true;
    }
    if let Some(home) = names.home_of(lib) {
        if let Some(name) = names.name_of(lib) {
            return push_in_home(names, store, at, home, Piece::Text(name), stack);
        }
        if let Some(header) = names.signature_text_of(lib).and_then(|t| t.impl_header) {
            return push_in_home(names, store, at, home, Piece::ImplHeader(header), stack);
        }
        if names.kind_of_item(lib) == Some(ItemKind::Const) {
            return push_in_home(names, store, at, home, Piece::Text("_"), stack);
        }
        return false;
    }
    if let Some(owner) = store.locals.block_owner(lib)
        && owner != at
    {
        stack.push(Piece::Path(owner));
        return true;
    }
    if let Some(path) = names.module_path(lib) {
        stack.push(Piece::ModPath { path, trailing_sep: false });
        return true;
    }
    false
}

/// Pushes `leaf` placed in `home`: under the owning body when `home` is a lowered block module,
/// else under `home`'s module path (with internal block-module segments stripped).
fn push_in_home<'a, 'n: 'a, S: NamesSource<'n>>(
    names: S,
    store: &'a Store<'n>,
    at: ItemKey,
    home: blaze_id::ItemKey,
    leaf: Piece<'a>,
    stack: &mut AVec<'_, Piece<'a>>,
) -> bool {
    if let Some(owner) = store.locals.block_owner(home)
        && owner != at
    {
        stack.push(leaf);
        stack.push(Piece::Text("::"));
        stack.push(Piece::Path(owner));
        return true;
    }
    let Some(path) = names.module_path(home) else {
        return false;
    };
    stack.push(leaf);
    stack.push(Piece::ModPath { path, trailing_sep: true });
    true
}

/// Pushes `<a, b>` onto `stack` in reverse reading order (nothing when `list` is empty).
fn push_ty_args_rev<'a>(stack: &mut AVec<'_, Piece<'a>>, list: &[PTy]) {
    if list.is_empty() {
        return;
    }
    stack.push(Piece::Text(">"));
    for (k, &p) in list.iter().enumerate().rev() {
        stack.push(Piece::Ty(p));
        if k > 0 {
            stack.push(Piece::Text(", "));
        }
    }
    stack.push(Piece::Text("<"));
}

/// Pushes `<a, b>` in reading order onto `seq` (nothing when `list` is empty).
fn push_ty_args_seq<'a>(seq: &mut AVec<'_, Piece<'a>>, list: &[PTy]) {
    if list.is_empty() {
        return;
    }
    seq.push(Piece::Text("<"));
    for (k, &p) in list.iter().enumerate() {
        if k > 0 {
            seq.push(Piece::Text(", "));
        }
        seq.push(Piece::Ty(p));
    }
    seq.push(Piece::Text(">"));
}

/// Formats one type node from `store.tys` (an impl's self type or trait argument). Generic
/// parameters and other non-ground placeholders print as `_`, matching rustc's `type_name`.
fn expand_ty<'a, 'n: 'a, S: NamesSource<'n>>(
    names: S,
    keys: &'a Keys,
    tys: PTys<'_>,
    t: PTy,
    out: &mut OwnString<blaze_own::Alloc<'static>>,
    seq: &mut AVec<'_, Piece<'a>>,
) -> bool {
    let Some(&node) = tys.get(t) else {
        return false;
    };
    match node {
        PNode::Bool => out.push_str("bool"),
        PNode::Char => out.push_str("char"),
        PNode::Str => out.push_str("str"),
        PNode::Never => out.push_str("!"),
        PNode::Int(i) => out.push_str(int_name(i)),
        PNode::Uint(u) => out.push_str(uint_name(u)),
        PNode::Float(f) => out.push_str(float_name(f)),
        PNode::Tuple(l) => {
            let Ok(parts) = tys.list(l) else { return false };
            seq.push(Piece::Text("("));
            for (k, &p) in parts.iter().enumerate() {
                if k > 0 {
                    seq.push(Piece::Text(", "));
                }
                seq.push(Piece::Ty(p));
            }
            seq.push(Piece::Text(if parts.len() == 1 { ",)" } else { ")" }));
        }
        PNode::Record(_, l) => {
            let Ok(parts) = tys.list(l) else { return false };
            seq.push(Piece::Text("record("));
            for (k, &p) in parts.iter().enumerate() {
                if k > 0 {
                    seq.push(Piece::Text(", "));
                }
                seq.push(Piece::Ty(p));
            }
            seq.push(Piece::Text(")"));
        }
        PNode::Union(l) => {
            let Ok(parts) = tys.list(l) else { return false };
            seq.push(Piece::Text("union("));
            for (k, &p) in parts.iter().enumerate() {
                if k > 0 {
                    seq.push(Piece::Text(" | "));
                }
                seq.push(Piece::Ty(p));
            }
            seq.push(Piece::Text(")"));
        }
        PNode::Ref(m, p) => {
            seq.push(Piece::Text(if m == Mutbl::Mut { "&mut " } else { "&" }));
            seq.push(Piece::Ty(p));
        }
        PNode::Ptr(m, p) => {
            seq.push(Piece::Text(if m == Mutbl::Mut { "*mut " } else { "*const " }));
            seq.push(Piece::Ty(p));
        }
        PNode::Array(e, n) => {
            seq.push(Piece::Text("["));
            seq.push(Piece::Ty(e));
            seq.push(Piece::Text("; "));
            seq.push(Piece::Ty(n));
            seq.push(Piece::Text("]"));
        }
        PNode::Slice(e) => {
            seq.push(Piece::Text("["));
            seq.push(Piece::Ty(e));
            seq.push(Piece::Text("]"));
        }
        PNode::Adt(item, l) | PNode::FnDef(item, l) => {
            let Ok(parts) = tys.list(l) else { return false };
            seq.push(Piece::Path(item));
            push_ty_args_seq(seq, parts);
        }
        PNode::Closure { owner, .. } | PNode::Coroutine { owner, .. } => {
            seq.push(Piece::Path(owner));
            seq.push(Piece::Text("::{{closure}}"));
        }
        PNode::FnPtr(l, header) => {
            let Ok(parts) = tys.list(l) else { return false };
            let Some((&output, inputs)) = parts.split_last() else { return false };
            let bits = header.bits();
            if bits & 0x80 != 0 {
                seq.push(Piece::Text("unsafe "));
            }
            match bits & 0x3f {
                0 => {}
                1 => seq.push(Piece::Text("extern \"C\" ")),
                2 => seq.push(Piece::Text("extern \"C-unwind\" ")),
                3 => seq.push(Piece::Text("extern \"system\" ")),
                4 => seq.push(Piece::Text("extern \"system-unwind\" ")),
                5 => seq.push(Piece::Text("extern \"rust-call\" ")),
                _ => return false,
            }
            seq.push(Piece::Text("fn("));
            for (k, &p) in inputs.iter().enumerate() {
                if k > 0 {
                    seq.push(Piece::Text(", "));
                }
                seq.push(Piece::Ty(p));
            }
            if header.c_variadic {
                seq.push(Piece::Text(if inputs.is_empty() { "..." } else { ", ..." }));
            }
            seq.push(Piece::Text(")"));
            let unit = matches!(tys.get(output), Some(PNode::Tuple(u)) if tys.list(*u).is_ok_and(|x| x.is_empty()));
            if !unit {
                seq.push(Piece::Text(" -> "));
                seq.push(Piece::Ty(output));
            }
        }
        PNode::Dynamic(l) => {
            let Ok(preds) = tys.list(l) else { return false };
            seq.push(Piece::Text("dyn "));
            let mut first = true;
            for &p in preds {
                let Some(&pred) = tys.get(p) else { return false };
                match pred {
                    PNode::ExTrait(item, al) => {
                        let Ok(a) = tys.list(al) else { return false };
                        seq.push(Piece::Path(item));
                        let binds = preds.iter().skip(1).any(|&q| matches!(tys.get(q), Some(PNode::ExProj { .. })));
                        if !a.is_empty() || binds {
                            seq.push(Piece::Text("<"));
                            for (k, &x) in a.iter().enumerate() {
                                if k > 0 {
                                    seq.push(Piece::Text(", "));
                                }
                                seq.push(Piece::Ty(x));
                            }
                            let mut k = a.len();
                            for &q in preds.iter().skip(1) {
                                if let Some(PNode::ExProj { name, term, .. }) = tys.get(q) {
                                    if k > 0 {
                                        seq.push(Piece::Text(", "));
                                    }
                                    let assoc = assoc_type_name(names, keys, item, *name).unwrap_or("_");
                                    seq.push(Piece::Text(assoc));
                                    seq.push(Piece::Text(" = "));
                                    seq.push(Piece::Ty(*term));
                                    k += 1;
                                }
                            }
                            seq.push(Piece::Text(">"));
                        }
                        first = false;
                    }
                    PNode::ExProj { .. } => {
                        if first {
                            return false;
                        }
                    }
                    PNode::ExAuto(item) => {
                        if !first {
                            seq.push(Piece::Text(" + "));
                        }
                        seq.push(Piece::Path(item));
                        first = false;
                    }
                    _ => return false,
                }
            }
        }
        PNode::ConstValue { ty, bits } => match tys.get(ty) {
            Some(PNode::Bool) => out.push_str(if bits != 0 { "true" } else { "false" }),
            Some(PNode::Uint(_)) => {
                let _ = write!(out, "{bits}");
            }
            Some(PNode::Int(i)) => {
                let b = bits.to_le_bytes();
                let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = b;
                match i {
                    IntTy::I8 => {
                        let _ = write!(out, "{}", i8::from_le_bytes([b0]));
                    }
                    IntTy::I16 => {
                        let _ = write!(out, "{}", i16::from_le_bytes([b0, b1]));
                    }
                    IntTy::I32 => {
                        let _ = write!(out, "{}", i32::from_le_bytes([b0, b1, b2, b3]));
                    }
                    IntTy::Isize | IntTy::I64 => {
                        let _ = write!(out, "{}", i64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]));
                    }
                    IntTy::I128 => {
                        let _ = write!(out, "{}", i128::from_le_bytes(b));
                    }
                }
            }
            Some(PNode::Char) => match u32::try_from(bits).ok().and_then(char::from_u32) {
                Some(c) => {
                    let _ = write!(out, "'{}'", c.escape_debug());
                }
                None => out.push_str("_"),
            },
            _ => out.push_str("_"),
        },
        PNode::Foreign(item) => seq.push(Piece::Path(item)),
        PNode::Pat(base, _) => seq.push(Piece::Ty(base)),
        PNode::Param(_)
        | PNode::ConstParam(_)
        | PNode::Infer
        | PNode::Alias { .. }
        | PNode::Opaque(..)
        | PNode::Future(..)
        | PNode::Error
        | PNode::ExTrait(..)
        | PNode::ExProj { .. }
        | PNode::ExAuto(_) => out.push_str("_"),
    }
    true
}

/// Finds the written name of associated type `name` declared by `trait_`.
fn assoc_type_name<'n, S: NamesSource<'n>>(names: S, keys: &Keys, trait_: ItemKey, name: Name) -> Option<&'n str> {
    let lib = keys.library_in(names, trait_)?;
    let mut found = None;
    names.each_member(lib, &mut |m| {
        if found.is_none()
            && m.kind == ItemKind::TypeAlias
            && let Some(n) = m.name
            && Name::of(n) == name
        {
            found = Some(n);
        }
    });
    found
}

/// Extracts the `impl ...` header before `{` from a body-local `impl`'s text.
fn local_impl_header(text: &str) -> Option<&str> {
    let head = text.split('{').next()?;
    let at = head.match_indices("impl").map(|(i, _)| i).find(|&i| {
        let before = head.get(..i).and_then(|s| s.chars().next_back());
        let after = head.get(i + 4..).and_then(|s| s.chars().next());
        before.is_none_or(|c| !(c.is_alphanumeric() || c == '_')) && after.is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
    })?;
    head.get(at..)
}

const fn int_name(i: IntTy) -> &'static str {
    match i {
        IntTy::Isize => "isize",
        IntTy::I8 => "i8",
        IntTy::I16 => "i16",
        IntTy::I32 => "i32",
        IntTy::I64 => "i64",
        IntTy::I128 => "i128",
    }
}

const fn uint_name(u: UintTy) -> &'static str {
    match u {
        UintTy::Usize => "usize",
        UintTy::U8 => "u8",
        UintTy::U16 => "u16",
        UintTy::U32 => "u32",
        UintTy::U64 => "u64",
        UintTy::U128 => "u128",
    }
}

const fn float_name(f: FloatTy) -> &'static str {
    match f {
        FloatTy::F16 => "f16",
        FloatTy::F32 => "f32",
        FloatTy::F64 => "f64",
        FloatTy::F128 => "f128",
    }
}
