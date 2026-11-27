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
/// Maximum visit steps per `walk_pty` call (Rule 18: bounds DAG/generic fan-out).
const MAX_WALK_STEPS: usize = 4096;

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
            visited.truncate(1);
            path_buf.clear();
            path_buf.push(field.name);
            let mut hit: Option<Hit> = None;
            walk_pty(reads, def.types, field.ty, &mut visited, &mut path_buf, &mut hit, alloc)?;
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
struct SubstNode<'t> {
    caller_types: PTys<'t>,
    args: &'t [PTy],
    parent: Option<usize>,
}

enum WalkStep<'t> {
    Visit {
        types: PTys<'t>,
        ty: PTy,
        subst: Option<usize>,
        depth: usize,
        borrowed: bool,
        via_adt: Option<ItemKey>,
    },
    PushField(Name),
    PopField,
    PopVisited { adt: ItemKey, monomorphic: bool },
}

fn walk_pty<'a, 't, W: World>(
    reads: &mut Reads<'t, 'a, W>,
    root_types: PTys<'t>,
    root_ty: PTy,
    visited: &mut Vec<ItemKey, Alloc<'a>>,
    path_buf: &mut Vec<Name, Alloc<'a>>,
    hit: &mut Option<Hit>,
    alloc: Alloc<'a>,
) -> Result<(), Stop> {
    let mut substs: Vec<SubstNode<'t>, Alloc<'a>> = Vec::new_in(alloc);
    let mut clean_mono: Vec<ItemKey, Alloc<'a>> = Vec::new_in(alloc);
    let mut work: Vec<WalkStep<'t>, Alloc<'a>> = Vec::new_in(alloc);
    let mut steps: usize = 0;
    work.push(WalkStep::Visit {
        types: root_types,
        ty: root_ty,
        subst: None,
        depth: 0,
        borrowed: false,
        via_adt: None,
    });

    while let Some(step) = work.pop() {
        match step {
            WalkStep::PushField(name) => path_buf.push(name),
            WalkStep::PopField => {
                let _ = path_buf.pop();
            }
            WalkStep::PopVisited { adt, monomorphic } => {
                let _ = visited.pop();
                let _ = substs.pop();
                if monomorphic && hit.is_none() && !clean_mono.iter().any(|&k| k == adt) {
                    clean_mono.push(adt);
                }
            }
            WalkStep::Visit { types, ty, subst, depth, borrowed, via_adt } => {
                if hit.is_some() || depth >= MAX_WALK_DEPTH || steps >= MAX_WALK_STEPS {
                    continue;
                }
                steps += 1;
                let Ok(node) = types.node(ty) else {
                    continue;
                };
                match node {
                    PNode::Param(ix) => {
                        if let Some(s_idx) = subst
                            && let Some(&s) = substs.get(s_idx)
                            && let Some(&arg_ty) = s.args.get(ix as usize)
                        {
                            work.push(WalkStep::Visit {
                                types: s.caller_types,
                                ty: arg_ty,
                                subst: s.parent,
                                depth: depth + 1,
                                borrowed,
                                via_adt,
                            });
                        }
                    }
                    PNode::Ref(_, inner) | PNode::Ptr(_, inner) => {
                        work.push(WalkStep::Visit {
                            types,
                            ty: inner,
                            subst,
                            depth: depth + 1,
                            borrowed: true,
                            via_adt,
                        });
                    }
                    PNode::Array(elem, _) | PNode::Slice(elem) | PNode::Pat(elem, _) => {
                        work.push(WalkStep::Visit {
                            types,
                            ty: elem,
                            subst,
                            depth: depth + 1,
                            borrowed,
                            via_adt,
                        });
                    }
                    PNode::Tuple(list) | PNode::Record(_, list) | PNode::Union(list) => {
                        if let Ok(elems) = types.list(list) {
                            for &elem in elems.iter().rev() {
                                work.push(WalkStep::Visit {
                                    types,
                                    ty: elem,
                                    subst,
                                    depth: depth + 1,
                                    borrowed,
                                    via_adt,
                                });
                            }
                        }
                    }
                    PNode::Adt(inner_adt, args_list) => {
                        let inner_attrs = reads.attrs(inner_adt)?;
                        if inner_attrs.is_capability() {
                            *hit = Some(Hit { capability: inner_adt, via_adt, borrowed });
                            break;
                        }
                        let arg_slice = types.list(args_list).ok().unwrap_or(&[]);
                        let next_via = via_adt.or(Some(inner_adt));
                        // 2. Queued first (runs after field walk): scan generic type arguments
                        //    directly for opaque/external wrappers whose bodies aren't populated
                        //    in `world` or hold `PhantomData<Cap>` (`Option<Cap>`, `Box<Cap>`, `Vec<Cap>`).
                        for &arg_ty in arg_slice.iter().rev() {
                            work.push(WalkStep::Visit {
                                types,
                                ty: arg_ty,
                                subst,
                                depth: depth + 1,
                                borrowed,
                                via_adt: next_via,
                            });
                        }
                        // 1. Queued last (runs first): inspect inner ADT's fields with generic
                        //    arguments substituted so `Wrap<Cap>` reports full nested field paths.
                        let monomorphic = arg_slice.is_empty();
                        if (!monomorphic || !clean_mono.iter().any(|&k| k == inner_adt))
                            && !visited.iter().any(|&k| k == inner_adt)
                            && let Some(inner_def) = reads.adt(inner_adt)?
                        {
                            visited.push(inner_adt);
                            let next_subst = substs.len();
                            substs.push(SubstNode { caller_types: types, args: arg_slice, parent: subst });
                            work.push(WalkStep::PopVisited { adt: inner_adt, monomorphic });
                            for variant in inner_def.variants.iter().rev() {
                                let Some(fields) = inner_def.fields.get(variant.fields.range()) else {
                                    continue;
                                };
                                for field in fields.iter().rev() {
                                    work.push(WalkStep::PopField);
                                    work.push(WalkStep::Visit {
                                        types: inner_def.types,
                                        ty: field.ty,
                                        subst: Some(next_subst),
                                        depth: depth + 1,
                                        borrowed,
                                        via_adt: next_via,
                                    });
                                    work.push(WalkStep::PushField(field.name));
                                }
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
            }
        }
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
