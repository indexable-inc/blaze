//! `capability-in-sans-io` (`#[blaze::capability]` firewall for `#![blaze::sans_io]` ADTs;
//! `docs/v2/plan/design/drop-with.md`, section 6):
//!
//! Any `struct`, `enum`, or `union` defined inside a `#![blaze::sans_io]` crate or module (or
//! marked `#[blaze::sans_io]`) must satisfy `NoCapability`: no field may directly or transitively
//! own or borrow a `#[blaze::capability]` type (`Alloc`, `BrandedAlloc`, `Fs`, `Net`, `Clock`,
//! `Mutex`, `GpuDevice`, etc.).
//!
//! - Fast path: O(1) bit check (`!attrs.is_sans_io() || attrs.is_capability()` -> zero work).
//! - Transitive walk: walks nested ADTs, generic type arguments (`Option<Cap>`, `Vec<Cap>`,
//!   `Wrap<Cap>`), references (`&Cap`, `&mut Cap`), raw pointers (`*const Cap`, `*mut Cap`),
//!   tuples, arrays, slices, pattern types, structural records, and structural unions.
//! - Function pointers (`fn(&Cap) -> T`, including `Brand<'id>`'s `PhantomData<fn(&'id ()) -> &'id ()>`)
//!   do NOT store capabilities and are never flagged.
//! - Cycle-safe & bounded (Rule 18): tracks the active ADT stack and caps recursion at
//!   [`MAX_WALK_DEPTH`] so recursive types (`struct Node { next: *const Node }`) terminate cleanly.

use blaze_own::{Alloc, Vec};

use crate::body::Expr;
use crate::diag::At;
use crate::error::{CheckError, Stop};
use crate::ids::{ExprId, ItemKey, Name};
use crate::machine::Machine;
use crate::portable::{PNode, PTy, PTys};
use crate::rules::args_inline::TypedFinding;
use crate::ty::{TyView, TypeCtx};
use crate::world::{AdtKind, Reads, World};

/// Maximum nesting depth for transitive capability field walks (Rule 18).
const MAX_WALK_DEPTH: usize = 32;

/// One violation of `blaze::capability_in_sans_io`: a `#[blaze::capability]` type stored
/// (directly or transitively, owned or borrowed) as a field of a `#![blaze::sans_io]` ADT.
#[derive(Debug, PartialEq, Eq)]
pub struct CapabilityFinding<'a> {
    /// The `#![blaze::sans_io]` ADT whose definition violates the capability firewall.
    pub adt: ItemKey,
    /// The offending `#[blaze::capability]` type reached from a field of `adt`.
    pub capability: ItemKey,
    /// Top-level field name on `adt` through which `capability` is reached.
    pub field: Name,
    /// Enclosing variant name if `adt` is an `enum` (empty/`None` for `struct`/`union`).
    pub variant: Option<Name>,
    /// Full field-name path from `adt` down to `capability` (e.g. `["inner", "scratch"]`).
    pub path: Vec<Name, Alloc<'a>>,
    /// Immediate intermediate ADT if `capability` was reached transitively through another ADT.
    pub via_adt: Option<ItemKey>,
    /// `true` if `capability` was reached behind a reference (`&T`/`&mut T`) or raw pointer.
    pub borrowed: bool,
}

/// Checks whether `adt` (if defined in `#![blaze::sans_io]` and not itself a
/// `#[blaze::capability]` definition) stores any `#[blaze::capability]` type directly or
/// transitively in any field.
///
/// Queries `world` through [`Reads`] so all `Question::Attrs` / `Question::Adt` dependencies are
/// recorded cleanly.
pub fn check_adt_capabilities<'a, W: World>(
    world: &W,
    adt: ItemKey,
    alloc: Alloc<'a>,
) -> Result<Vec<CapabilityFinding<'a>, Alloc<'a>>, CheckError> {
    let mut reads = Reads::new_in(world, alloc);
    match check_adt_capabilities_in(&mut reads, adt, alloc) {
        Ok(findings) => Ok(findings),
        Err(Stop::Error(e)) => Err(e),
        Err(Stop::Need(_)) => Ok(Vec::new_in(alloc)),
    }
}

/// Shared sans-IO core driven by both standalone [`check_adt_capabilities`] and body typeck
/// ([`Machine::capability_in_sans_io`]).
pub(crate) fn check_adt_capabilities_in<'a, W: World>(
    reads: &mut Reads<'_, 'a, W>,
    adt: ItemKey,
    alloc: Alloc<'a>,
) -> Result<Vec<CapabilityFinding<'a>, Alloc<'a>>, Stop> {
    let mut out: Vec<CapabilityFinding<'a>, Alloc<'a>> = Vec::new_in(alloc);
    let attrs = reads.attrs(adt)?;
    // Fast path: non-sans-IO ADTs and `#[blaze::capability]` definitions themselves (e.g.
    // `BrandedAlloc` wrapping `Alloc`) are allowed to contain capabilities.
    if !attrs.is_sans_io() || attrs.is_capability() {
        return Ok(out);
    }
    let Some(def) = reads.adt(adt)? else {
        return Ok(out);
    };
    let is_enum = def.kind == AdtKind::Enum;
    let mut visited: Vec<ItemKey, Alloc<'a>> = Vec::new_in(alloc);
    visited.push(adt);
    let mut path_buf: Vec<Name, Alloc<'a>> = Vec::new_in(alloc);

    for variant in def.variants {
        let variant_name = is_enum.then_some(variant.name);
        let Some(fields) = def.fields.get(variant.fields.range()) else {
            continue;
        };
        for field in fields {
            path_buf.clear();
            path_buf.push(field.name);
            let mut hit: Option<Hit> = None;
            walk_pty(
                reads,
                def.types,
                field.ty,
                None,
                0,
                false,
                None,
                &mut visited,
                &mut path_buf,
                &mut hit,
                alloc,
            )?;
            if let Some(hit) = hit {
                let mut saved_path: Vec<Name, Alloc<'a>> = Vec::with_capacity_in(path_buf.len(), alloc);
                saved_path.extend_from_slice(path_buf.as_slice());
                out.push(CapabilityFinding {
                    adt,
                    capability: hit.capability,
                    field: field.name,
                    variant: variant_name,
                    path: saved_path,
                    via_adt: hit.via_adt,
                    borrowed: hit.borrowed,
                });
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Copy)]
struct Hit {
    capability: ItemKey,
    via_adt: Option<ItemKey>,
    borrowed: bool,
}

/// Active substitution environment when descending into a generic ADT `Foo<A, B>` whose field types
/// reference `PNode::Param(i)` in `owner_types`: maps parameter `i` back to `(caller_types, args[i])`.
#[derive(Clone, Copy)]
struct Subst<'t> {
    caller_types: PTys<'t>,
    args: &'t [PTy],
    parent: Option<&'t Subst<'t>>,
}

#[expect(clippy::too_many_arguments, reason = "explicit sans-IO traversal state without ambient allocators")]
fn walk_pty<'a, 't, W: World>(
    reads: &mut Reads<'_, 'a, W>,
    types: PTys<'t>,
    ty: PTy,
    subst: Option<&Subst<'t>>,
    depth: usize,
    borrowed: bool,
    via_adt: Option<ItemKey>,
    visited: &mut Vec<ItemKey, Alloc<'a>>,
    path_buf: &mut Vec<Name, Alloc<'a>>,
    hit: &mut Option<Hit>,
    alloc: Alloc<'a>,
) -> Result<(), Stop> {
    if hit.is_some() || depth >= MAX_WALK_DEPTH {
        return Ok(());
    }
    let Ok(node) = types.node(ty) else {
        return Ok(());
    };
    match node {
        PNode::Param(ix) => {
            if let Some(s) = subst
                && let Some(&arg_ty) = s.args.get(ix as usize)
            {
                walk_pty(
                    reads,
                    s.caller_types,
                    arg_ty,
                    s.parent,
                    depth + 1,
                    borrowed,
                    via_adt,
                    visited,
                    path_buf,
                    hit,
                    alloc,
                )?;
            }
        }
        PNode::Ref(_, inner) | PNode::Ptr(_, inner) => {
            walk_pty(reads, types, inner, subst, depth + 1, true, via_adt, visited, path_buf, hit, alloc)?;
        }
        PNode::Array(elem, _) | PNode::Slice(elem) | PNode::Pat(elem, _) => {
            walk_pty(reads, types, elem, subst, depth + 1, borrowed, via_adt, visited, path_buf, hit, alloc)?;
        }
        PNode::Tuple(list) | PNode::Record(_, list) | PNode::Union(list) => {
            if let Ok(elems) = types.list(list) {
                for &elem in elems {
                    walk_pty(reads, types, elem, subst, depth + 1, borrowed, via_adt, visited, path_buf, hit, alloc)?;
                    if hit.is_some() {
                        return Ok(());
                    }
                }
            }
        }
        PNode::Adt(inner_adt, args_list) => {
            let inner_attrs = reads.attrs(inner_adt)?;
            if inner_attrs.is_capability() {
                *hit = Some(Hit { capability: inner_adt, via_adt, borrowed });
                return Ok(());
            }
            let arg_slice = types.list(args_list).ok().unwrap_or(&[]);
            // 1. Inspect inner ADT's fields with generic arguments substituted (so `Wrap<Cap>`
            //    reports the full nested field path `["w", "x"]` before falling back to arg scan).
            let next_via = via_adt.or(Some(inner_adt));
            if !visited.iter().any(|&k| k == inner_adt)
                && let Some(inner_def) = reads.adt(inner_adt)?
            {
                visited.push(inner_adt);
                let next_subst = Subst { caller_types: types, args: arg_slice, parent: subst };
                'variants: for variant in inner_def.variants {
                    let Some(fields) = inner_def.fields.get(variant.fields.range()) else {
                        continue;
                    };
                    for field in fields {
                        let mark = path_buf.len();
                        path_buf.push(field.name);
                        walk_pty(
                            reads,
                            inner_def.types,
                            field.ty,
                            Some(&next_subst),
                            depth + 1,
                            borrowed,
                            next_via,
                            visited,
                            path_buf,
                            hit,
                            alloc,
                        )?;
                        if hit.is_some() {
                            break 'variants;
                        }
                        path_buf.truncate(mark);
                    }
                }
                visited.pop();
                if hit.is_some() {
                    return Ok(());
                }
            }
            // 2. Also scan generic type arguments directly for opaque/external wrappers whose
            //    bodies aren't populated in `world` or hold `PhantomData<Cap>` (e.g. `Option<Cap>`,
            //    `Box<Cap>`, `Vec<Cap>`).
            for &arg_ty in arg_slice {
                walk_pty(reads, types, arg_ty, subst, depth + 1, borrowed, next_via, visited, path_buf, hit, alloc)?;
                if hit.is_some() {
                    return Ok(());
                }
            }
        }
        // Function pointers (`fn(&Cap) -> T`, including `Brand<'id>`'s `fn(&'id ()) -> &'id ()`)
        // take capabilities as call parameters rather than storing them.
        PNode::FnPtr(..)
        | PNode::FnDef(..)
        | PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Str
        | PNode::Never
        | PNode::Closure { .. }
        | PNode::Future(..)
        | PNode::Coroutine { .. }
        | PNode::Alias { .. }
        | PNode::Opaque(..)
        | PNode::Infer
        | PNode::Error
        | PNode::ConstValue { .. }
        | PNode::ConstParam(_)
        | PNode::Dynamic(_)
        | PNode::ExTrait(..)
        | PNode::ExProj { .. }
        | PNode::ExAuto(_)
        | PNode::Foreign(_) => {}
    }
    Ok(())
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Body-level `capability-in-sans-io` check after the end sequence: checks every ADT
    /// constructed via a struct literal in the body (once per distinct ADT) and emits a
    /// [`TypedFinding::CapabilityInSansIo`] for any `#![blaze::sans_io]` ADT storing a
    /// `#[blaze::capability]` field directly or transitively.
    pub(crate) fn capability_in_sans_io(&mut self) -> Result<(), Stop> {
        let alloc = self.alloc;
        let mut checked: Vec<ItemKey, Alloc<'a>> = Vec::new_in(alloc);
        for (i, &x) in self.body.exprs.iter().enumerate() {
            if !matches!(x, Expr::Struct { .. }) {
                continue;
            }
            let e = ExprId(i as u32);
            let t = self.expr_ty[e.ix()];
            let TyView::Adt(adt, _) = self.view(t) else {
                continue;
            };
            if checked.iter().any(|&k| k == adt) {
                continue;
            }
            checked.push(adt);
            let findings = check_adt_capabilities_in(&mut self.reads, adt, alloc)?;
            let at = self.range_of(At::Expr(e));
            for f in findings {
                self.typed_findings.push(TypedFinding::CapabilityInSansIo {
                    adt: f.adt,
                    capability: f.capability,
                    field: f.field,
                    via_adt: f.via_adt,
                    borrowed: f.borrowed,
                    at,
                });
            }
        }
        Ok(())
    }
}
