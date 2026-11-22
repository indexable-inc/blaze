//! The tests' toy world: structs with fields, the owner fn's signature, and answers that can be
//! perturbed and can be held back (typeck.md, "Testing"). The type context is the real one,
//! `InferCtx`; the toy type context was deleted when blaze-ladder switched to it.

use blaze_own::{Alloc, Vec};
use std::alloc::Global;

use crate::error::CheckError;
use crate::ids::{ItemKey, LangItem, List, Name, PathIdx, ScopeId};
use crate::portable::{PNode, PTy, PTys, PortableTys};
use crate::world::{
    Adt, AdtKind, AssocItem, CtorKind, Def, Fetch, Field, FnSig, Hidden, ImplHeader, Missing, PClause, PathRes,
    Predicates, Question, Seal, Shape, TypeAttrs, Variant, World,
};

/// A field's type among the toy's few.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldTy {
    I32,
    U8,
    Bool,
    /// Another struct of the world, by index.
    Struct(u32),
}

/// One struct of the world: its fields' names and types.
pub(crate) struct ToyStruct {
    pub fields: Vec<(Name, FieldTy), Global>,
}

/// One answer table per question, owned.
struct Owned {
    nodes: Vec<PNode, Global>,
    lists: Vec<PTy, Global>,
}

impl Owned {
    fn view(&self) -> PTys<'_> {
        PTys::flat(&self.nodes, &self.lists)
    }
}

struct AdtAnswer {
    variants: Vec<Variant, Global>,
    fields: Vec<Field, Global>,
    types: Owned,
}

/// The toy world: the body's owner (a fn), its parameters' and output's types, structs, and
/// the questions it holds back.
pub(crate) struct ToyWorld {
    pub owner: ItemKey,
    sig_inputs: Vec<PTy, Global>,
    sig_output: PTy,
    sig_types: Owned,
    adts: Vec<(ItemKey, AdtAnswer), Global>,
    /// The opaques this world reveals, each with its hidden type (node 0 of its own table);
    /// every other opaque is seen as itself (`opaque_hidden` answers `None`).
    revealed: Vec<(ItemKey, Owned), Global>,
    /// Questions answered `Missing`.
    pub missing: Vec<Question, Global>,
}

/// The key of struct `i`.
pub(crate) fn struct_key(i: u32) -> ItemKey {
    ItemKey(0x5_0000 + u128::from(i))
}

fn to_owned(t: PortableTys<'_>) -> Owned {
    let v = t.view();
    let mut nodes = Vec::with_capacity_in(v.len(), Global);
    nodes.extend(v.nodes().copied());
    let mut lists = Vec::with_capacity_in(v.lists_len(), Global);
    lists.extend(v.lists());
    Owned { nodes, lists }
}

fn field_pty(t: &mut PortableTys<'_>, f: FieldTy) -> Result<PTy, CheckError> {
    t.intern(match f {
        FieldTy::I32 => PNode::Int(crate::ids::IntTy::I32),
        FieldTy::U8 => PNode::Uint(crate::ids::UintTy::U8),
        FieldTy::Bool => PNode::Bool,
        FieldTy::Struct(j) => PNode::Adt(struct_key(j), List::EMPTY),
    })
}

impl ToyWorld {
    /// A world whose owner takes one parameter of each type in `params` and returns `output`.
    pub(crate) fn new(structs: &[ToyStruct], params: &[FieldTy], output: FieldTy) -> Result<ToyWorld, CheckError> {
        let mut sig = PortableTys::new_in(Alloc::global());
        let mut sig_inputs = Vec::new_in(Global);
        for &p in params {
            sig_inputs.push(field_pty(&mut sig, p)?);
        }
        let sig_output = field_pty(&mut sig, output)?;
        let mut adts = Vec::new_in(Global);
        for (i, s) in structs.iter().enumerate() {
            let mut t = PortableTys::new_in(Alloc::global());
            let mut fields = Vec::new_in(Global);
            for &(name, f) in s.fields.iter() {
                fields.push(Field { name, ty: field_pty(&mut t, f)? });
            }
            let mut variants = Vec::new_in(Global);
            variants.push(Variant {
                key: struct_key(i as u32),
                name: Name(i as u64),
                ctor: CtorKind::Braces,
                fields: List { start: 0, len: fields.len() as u32 },
            });
            adts.push((struct_key(i as u32), AdtAnswer { variants, fields, types: to_owned(t) }));
        }
        Ok(ToyWorld {
            owner: ItemKey(1),
            sig_inputs,
            sig_output,
            sig_types: to_owned(sig),
            adts,
            revealed: Vec::new_in(Global),
            missing: Vec::new_in(Global),
        })
    }

    /// Reveals `opaque` to this world's checks with hidden type `ty` (a scalar: one node).
    pub(crate) fn reveal(&mut self, opaque: ItemKey, ty: FieldTy) -> Result<(), CheckError> {
        let mut t = PortableTys::new_in(blaze_own::testing::heap().alloc());
        field_pty(&mut t, ty)?;
        self.revealed.retain(|(k, _)| *k != opaque);
        self.revealed.push((opaque, to_owned(t)));
        Ok(())
    }

    fn held(&self, q: Question) -> Fetch<()> {
        if self.missing.contains(&q) { Err(Missing) } else { Ok(()) }
    }

    /// Changes struct `i`'s answer: every field's type becomes the next of i32, u8, bool.
    pub(crate) fn perturb(&mut self, i: u32) {
        let Some((_, a)) = self.adts.iter_mut().find(|(k, _)| *k == struct_key(i)) else { return };
        for n in a.types.nodes.iter_mut() {
            *n = match *n {
                PNode::Int(_) => PNode::Uint(crate::ids::UintTy::U8),
                PNode::Uint(_) => PNode::Bool,
                PNode::Bool => PNode::Int(crate::ids::IntTy::I32),
                other => other,
            };
        }
    }
}

impl World for ToyWorld {
    fn fn_sig(&self, _: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        self.held(Question::FnSig(item))?;
        if item != self.owner {
            return Ok(None);
        }
        Ok(Some(FnSig {
            parent_count: 0,
            own_count: 0,
            declared_count: 0,
            self_param: false,
            header: crate::ids::FnPtrHeader::RUST,
            c_variadic: false,
            legacy_const_generics: &[],
            inputs: &self.sig_inputs,
            output: self.sig_output,
            types: self.sig_types.view(),
        }))
    }

    fn predicates(&self, _: Seal, item: ItemKey) -> Fetch<Predicates<'_>> {
        self.held(Question::Predicates(item))?;
        Ok(Predicates { clauses: &[] as &[PClause], types: PTys::EMPTY })
    }

    fn adt(&self, _: Seal, item: ItemKey) -> Fetch<Option<Adt<'_>>> {
        self.held(Question::Adt(item))?;
        Ok(self.adts.iter().find(|(k, _)| *k == item).map(|(_, a)| Adt {
            kind: AdtKind::Struct,
            generics: 0,
            variants: &a.variants,
            fields: &a.fields,
            types: a.types.view(),
        }))
    }

    fn impls_of(&self, _: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
        self.held(Question::ImplsOf(trait_, shape))?;
        Ok(&[])
    }

    fn impl_header(&self, _: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        self.held(Question::ImplHeader(item))?;
        Ok(None)
    }

    fn inherent(&self, _: Seal, shape: Shape, name: Name) -> Fetch<&[AssocItem]> {
        self.held(Question::Inherent(shape, name))?;
        Ok(&[])
    }

    fn assoc_item(&self, _: Seal, container: ItemKey, name: Name) -> Fetch<Option<AssocItem>> {
        self.held(Question::AssocItem(container, name))?;
        Ok(None)
    }

    fn traits_in_scope(&self, _: Seal, scope: ScopeId) -> Fetch<&[ItemKey]> {
        self.held(Question::TraitsInScope(scope))?;
        Ok(&[])
    }

    fn resolve_path(&self, _: Seal, scope: ScopeId, path: PathIdx, segments: &[Name]) -> Fetch<crate::path_miss::PathAnswer> {
        self.held(Question::Path(scope, path))?;
        // `S<i>` names struct `i`, by the name `Name(i)` its variant has; a one-segment name
        // that is none is its first segment's miss. The toy names no longer path.
        Ok(match segments {
            [n] => self
                .adts
                .iter()
                .position(|(_, a)| a.variants[0].name == *n)
                .map(|i| PathRes { def: Def::Adt(struct_key(i as u32)), consumed: 1 })
                .ok_or(crate::path_miss::PathMiss::FIRST),
            _ => Err(crate::path_miss::PathMiss::Whole),
        })
    }

    fn lang_item(&self, _: Seal, item: LangItem) -> Fetch<Option<ItemKey>> {
        self.held(Question::Lang(item))?;
        Ok(None)
    }

    fn lang_path(&self, _: Seal, item: LangItem) -> Fetch<Option<Def>> {
        self.held(Question::LangPath(item))?;
        Ok(None)
    }

    fn const_value(&self, _: Seal, item: ItemKey) -> Fetch<Option<u64>> {
        self.held(Question::Const(item))?;
        Ok(None)
    }

    fn opaque_hidden(&self, _: Seal, item: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        self.held(Question::Opaque(item))?;
        Ok(self.revealed.iter().find(|(k, _)| *k == item).map(|(_, t)| Hidden { ty: PTy(0), types: t.view() }))
    }
    fn closure(&self, _: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<crate::world::ClosureFacts<'_>>> {
        self.held(Question::Closure(owner, ordinal))?;
        Ok(None)
    }
    fn attrs(&self, _: Seal, item: ItemKey) -> Fetch<TypeAttrs> {
        self.held(Question::Attrs(item))?;
        Ok(TypeAttrs(0))
    }

    fn trait_def(&self, _: Seal, item: ItemKey) -> Fetch<Option<crate::world::TraitDef<'_>>> {
        self.held(Question::TraitDef(item))?;
        Ok(None)
    }

    fn assoc_parent(&self, _: Seal, item: ItemKey) -> Fetch<Option<crate::world::AssocParent>> {
        self.held(Question::AssocParent(item))?;
        Ok(None)
    }

    /// The toy declares no `where const` condition: one asked about is an evaluation error.
    fn const_holds(&self, _: Seal, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Fetch<crate::world::ConstHolds> {
        self.held(Question::ConstHolds(cond, crate::world::ground_args_digest(args, types)))?;
        Ok(crate::world::ConstHolds::Error)
    }

    /// The toy's items have type parameters only.
    fn const_params(&self, _: Seal, item: ItemKey) -> Fetch<crate::world::ConstParams<'_>> {
        self.held(Question::ConstParams(item))?;
        Ok(crate::world::ConstParams { params: &[], types: PTys::EMPTY })
    }
}
