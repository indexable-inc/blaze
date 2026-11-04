//! The crate walk, and the ids of what is translated.
//!
//! Items are taken in one fixed order, and ids handed out in it: the modules in the def map's
//! order, in each the declarations in order. Type `0` is the tuple, as in charon-driver; then
//! each local struct and enum; function ids go to the free functions, then to the methods of the
//! inherent impls (an impl's self type may be declared in a module walked after it), and only
//! then to what the bodies call from other crates and to the overflow intrinsics, on first use.

use std::alloc::Global;

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use charon_lib::ast::{
    Body, DeBruijnVar, File, FileId, FileName, FloatTy, FunDecl, FunDeclId, FunSig, FunSource,
    GenericParams as CGenericParams, IntTy as CIntTy, IntegerTy, ItemByVal, ItemId, ItemMeta,
    ItemOpacity, RuntimeChecks, ScalarTy, SeqHashMap, Span, TargetInfo, TranslatedCrate, Ty as CTy,
    TyKind as CTyKind, TypeDeclId, TypeParam, TypeVarId, UIntTy as CUIntTy, Variance,
    from_rustc::{Arch, BinaryFormat, CfgAbi, Env, Os},
};
use charon_lib::ids::IndexVec;
use hir_def::{
    AdtId, AssocItemId, FunctionId, GenericDefId, HasModule as _, ImplId, ItemContainerId,
    Lookup as _, ModuleDefId,
    hir::generics::GenericParams,
    nameres::crate_def_map,
    signatures::{
        ConstSignature, FunctionSignature, ImplSignature, StaticSignature, TraitSignature,
        TypeAliasSignature,
    },
};
use hir_ty::{db::HirDatabase as _, next_solver::DbInterner};

use crate::{
    Unsupported, bodies,
    names::{self, ImplOf, ItemPath},
    refuse,
    signatures::{self, Signature},
    types::{self, Regions},
    why,
};

/// The three overflowing operations a body computes with an intrinsic, as charon-driver does
/// (`core::intrinsics::{add,sub,mul}_with_overflow`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Overflowing {
    Add,
    Sub,
    Mul,
}

impl Overflowing {
    fn intrinsic(self) -> &'static str {
        match self {
            Overflowing::Add => "add_with_overflow",
            Overflowing::Sub => "sub_with_overflow",
            Overflowing::Mul => "mul_with_overflow",
        }
    }
}

/// What the translation of one crate shares: the database, the crate being built and the ids.
pub(crate) struct Ctx<'db> {
    pub(crate) db: &'db Db,
    pub(crate) interner: DbInterner<'db>,
    pub(crate) krate: Crate,
    /// Whether the target stores integers little-endian: how constant bytes read.
    pub(crate) little_endian: bool,
    pub(crate) translated: TranslatedCrate,
    /// Every local struct and enum, with its id.
    pub(crate) adts: HashMap<AdtId, TypeDeclId, Global>,
    /// Every function with an id: the local ones walked, and the foreign ones called.
    funs: HashMap<FunctionId, FunDeclId, Global>,
    /// The number of region parameters of each function declared: what a call passes.
    regions: HashMap<FunDeclId, usize, Global>,
    overflowing: [Option<FunDeclId>; 3],
}

/// A local function to translate.
struct LocalFn {
    func: FunctionId,
    id: FunDeclId,
    path: ItemPath,
    src: FunSource,
}

/// The crate, as ULLBC; see [`crate::translate_crate`].
pub(crate) fn translate(db: &Db, krate: Crate) -> Result<TranslatedCrate, Vec<Unsupported>> {
    let crate_name = names::crate_name(db, krate);
    let mut errors: Vec<Unsupported> = Vec::new(); // alloc: errors, once per crate
    let layout = match db.target_data_layout(krate) {
        Ok(layout) => layout,
        Err(err) => {
            // alloc: an error
            return Err(vec![refuse(&crate_name, format!("the target ({err})"), why::TARGET)]);
        }
    };
    let mut translated = TranslatedCrate {
        crate_name: crate_name.clone(), // clone: a short name
        ..TranslatedCrate::default()
    };
    match target_info(db, krate, layout) {
        Ok((triple, info)) => {
            translated.target_information.insert(triple, info);
        }
        Err(err) => errors.push(refuse(&crate_name, err, why::TARGET)),
    }
    let cfg = krate.cfg_options(db);
    translated.runtime_checks = RuntimeChecks {
        overflow_checks: cfg
            .check_atom(&cfg::CfgAtom::Flag(intern::Symbol::intern("overflow_checks"))),
        ub_checks: cfg.check_atom(&cfg::CfgAtom::Flag(intern::Symbol::intern("debug_assertions"))),
        contract_checks: cfg.contract_checks(),
    };
    // Every span is `Span::dummy()`, file 0: the file is named so a reader can resolve it.
    translated.files.push(File {
        id: FileId::ZERO,
        name: FileName::NotReal(crate_name.clone()), // clone: a short name
        crate_name: crate_name.clone(),              // clone: a short name
        contents: None,
    });
    // The tuple, as the Aeneas preset (`no_gen_tuple_structs`) leaves it: its id and name, no
    // declaration.
    let unit = translated.type_decls.reserve_slot();
    debug_assert_eq!(unit, TypeDeclId::UNIT);
    translated.item_names.insert(ItemId::Type(unit), names::tuple_name());

    let mut ctx = Ctx {
        db,
        interner: DbInterner::new_with(db, krate),
        krate,
        little_endian: matches!(layout.endian, rustc_abi::Endian::Little),
        translated,
        adts: HashMap::default(),    // alloc: the crate's ids
        funs: HashMap::default(),    // alloc: the crate's ids
        regions: HashMap::default(), // alloc: the crate's ids
        overflowing: [None; 3],
    };

    // The walk: ids for the structs, enums and free functions, refusals for everything else.
    let mut adts = Vec::new(); // alloc: the crate's ADTs, once
    let mut locals = Vec::new(); // alloc: the crate's functions, once
    let mut impls = Vec::new(); // alloc: the crate's impls, once
    let def_map = crate_def_map(db, krate);
    for (module, data) in def_map.modules(db) {
        let scope = data.scope(db);
        let modules = names::module_segments(db, module);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(func) => match func.lookup(db).container {
                    ItemContainerId::ModuleId(_) => {
                        let name = FunctionSignature::of(db, func).name.as_str().to_owned(); // alloc: a name
                        let path = names::item_path(&crate_name, &modules, None, &name);
                        let id = ctx.translated.fun_decls.reserve_slot();
                        ctx.funs.insert(func, id);
                        locals.push(LocalFn { func, id, path, src: FunSource::Normal });
                    }
                    _ => {
                        let name = FunctionSignature::of(db, func).name.as_str().to_owned(); // alloc: a name
                        let path = names::item_path(&crate_name, &modules, None, &name);
                        errors.push(refuse(
                            &path.display,
                            "a function in an extern block",
                            why::UNSAFE,
                        ));
                    }
                },
                ModuleDefId::AdtId(adt @ (AdtId::StructId(_) | AdtId::EnumId(_))) => {
                    let path = names::adt_path(db, adt);
                    let params = GenericParams::of(db, GenericDefId::AdtId(adt));
                    if !params.is_empty() || !params.has_no_predicates() {
                        errors.push(refuse(
                            &path.display,
                            "a generic struct or enum",
                            why::GENERICS,
                        ));
                        continue;
                    }
                    let id = ctx.translated.type_decls.reserve_slot();
                    ctx.adts.insert(adt, id);
                    adts.push((adt, id, path));
                }
                ModuleDefId::AdtId(adt @ AdtId::UnionId(_)) => {
                    errors.push(refuse(&names::adt_path(db, adt).display, "a union", why::TYPES));
                }
                ModuleDefId::ConstId(it) => {
                    let name = ConstSignature::of(db, it)
                        .name
                        .as_ref()
                        .map_or("_", |name| name.as_str())
                        .to_owned(); // alloc: a name
                    let path = names::item_path(&crate_name, &modules, None, &name);
                    errors.push(refuse(&path.display, "a constant item", why::ITEMS));
                }
                ModuleDefId::StaticId(it) => {
                    let name = StaticSignature::of(db, it).name.as_str().to_owned(); // alloc: a name
                    let path = names::item_path(&crate_name, &modules, None, &name);
                    errors.push(refuse(&path.display, "a static", why::UNSAFE));
                }
                ModuleDefId::TraitId(it) => {
                    let name = TraitSignature::of(db, it).name.as_str().to_owned(); // alloc: a name
                    let path = names::item_path(&crate_name, &modules, None, &name);
                    errors.push(refuse(&path.display, "a trait", why::TRAITS));
                }
                ModuleDefId::TypeAliasId(it) => {
                    let name = TypeAliasSignature::of(db, it).name.as_str().to_owned(); // alloc: a name
                    let path = names::item_path(&crate_name, &modules, None, &name);
                    errors.push(refuse(&path.display, "a type alias", why::ITEMS));
                }
                // Modules are walked on their own; variants come with their enum; built-in
                // types and macros are not items of the crate's code.
                ModuleDefId::ModuleId(_)
                | ModuleDefId::EnumVariantId(_)
                | ModuleDefId::BuiltinType(_)
                | ModuleDefId::MacroId(_) => {}
            }
        }
        if scope.extern_blocks().next().is_some() {
            let path = names::item_path(&crate_name, &modules, None, "extern");
            errors.push(refuse(&path.display, "an extern block", why::UNSAFE));
        }
        for imp in scope.impls() {
            impls.push((imp, modules.clone())); // clone: a module path, per impl
        }
    }
    for (imp, modules) in impls {
        match inherent_impl(&mut ctx, &crate_name, &modules, imp) {
            Ok(Some(impl_of)) => {
                for (name, item) in imp.impl_items(db).items.iter() {
                    let path =
                        names::item_path(&crate_name, &modules, Some(&impl_of), name.as_str());
                    match *item {
                        AssocItemId::FunctionId(func) => {
                            let id = ctx.translated.fun_decls.reserve_slot();
                            ctx.funs.insert(func, id);
                            let self_ty = impl_of.self_ty.clone(); // clone: a hash-consed handle
                            locals.push(LocalFn {
                                func,
                                id,
                                path,
                                src: FunSource::InherentImpl { self_ty },
                            });
                        }
                        AssocItemId::ConstId(_) => {
                            errors.push(refuse(
                                &path.display,
                                "an associated constant",
                                why::ITEMS,
                            ));
                        }
                        AssocItemId::TypeAliasId(_) => {
                            errors.push(refuse(&path.display, "an associated type", why::TRAITS));
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(err) => errors.push(err),
        }
    }

    // The declarations, in id order.
    for (adt, id, path) in adts {
        match types::adt_decl(&mut ctx, adt, id, &path) {
            Ok(decl) => ctx.translated.set_new_item_slot(ItemId::Type(id), ItemByVal::Type(decl)),
            Err(refused) => errors.extend(refused),
        }
    }
    // Every local signature before any body: a call passes its callee's region count.
    let mut signatures = Vec::with_capacity(locals.len()); // alloc: the crate's signatures, once
    for local in &locals {
        match signatures::signature(&mut ctx, local.func, &local.path.display) {
            Ok(sig) => {
                ctx.regions.insert(local.id, sig.region_count());
                signatures.push(Some(sig));
            }
            Err(refused) => {
                errors.extend(refused);
                signatures.push(None);
            }
        }
    }
    for (local, sig) in locals.into_iter().zip(signatures) {
        let body = bodies::body(&mut ctx, local.func, &local.path.display);
        match (sig, body) {
            (Some(Signature { generics, sig }), Ok(body)) => {
                let decl = FunDecl {
                    def_id: local.id,
                    item_meta: local_meta(local.path.name),
                    generics,
                    signature: Box::new(sig), // alloc: a Charon signature
                    src: local.src,
                    body: Body::Unstructured(body),
                };
                ctx.translated.set_new_item_slot(ItemId::Fun(local.id), ItemByVal::Fun(decl));
            }
            (_, Err(refused)) => errors.extend(refused),
            (None, Ok(_)) => {}
        }
    }

    if !errors.is_empty() {
        errors.sort();
        errors.dedup();
        return Err(errors);
    }
    Ok(ctx.translated)
}

/// The impl block `imp` if it is inherent and non-generic, `None` when it has no items; a
/// refusal for a trait impl or a generic one.
fn inherent_impl<'db>(
    ctx: &mut Ctx<'db>,
    crate_name: &str,
    modules: &[String],
    imp: ImplId,
) -> Result<Option<ImplOf>, Unsupported> {
    let self_ty = ctx.db.impl_self_ty(imp).skip_binder();
    let display = match self_ty_name(ctx.db, self_ty) {
        Some(name) => name,
        None => format!("{self_ty:?}"), // alloc: a name, once per impl
    };
    let path = names::item_path(crate_name, modules, None, &format!("<impl {display}>")); // alloc: a name
    if ImplSignature::of(ctx.db, imp).target_trait.is_some() {
        return Err(refuse(&path.display, "an impl of a trait", why::TRAITS));
    }
    let params = GenericParams::of(ctx.db, GenericDefId::ImplId(imp));
    if !params.is_empty() || !params.has_no_predicates() {
        return Err(refuse(&path.display, "a generic impl", why::GENERICS));
    }
    if imp.impl_items(ctx.db).items.is_empty() {
        return Ok(None);
    }
    let self_ty = types::ty(ctx, &path.display, self_ty, &mut Regions::Decl)?;
    Ok(Some(ImplOf { self_ty, display }))
}

/// An ADT self type's name, for paths.
fn self_ty_name(db: &Db, self_ty: hir_ty::next_solver::Ty<'_>) -> Option<String> {
    use rustc_type_ir::inherent::{AdtDef as _, IntoKind as _};
    match self_ty.kind() {
        hir_ty::next_solver::TyKind::Adt(def, _) => Some(names::adt_name(db, def.def_id())),
        _ => None,
    }
}

impl<'db> Ctx<'db> {
    /// The id of the function a body calls, `caller` the caller's path for errors: a local
    /// function walked, or a foreign one declared without a body on first use.
    pub(crate) fn callee(
        &mut self,
        caller: &str,
        func: FunctionId,
    ) -> Result<FunDeclId, Unsupported> {
        if let Some(&id) = self.funs.get(&func) {
            return Ok(id);
        }
        let db = self.db;
        let crate_name = names::crate_name(db, func.krate(db));
        let modules = names::module_segments(db, func.module(db));
        let name = FunctionSignature::of(db, func).name.as_str().to_owned(); // alloc: a name
        let local = func.krate(db) == self.krate;
        let (path, src) = match func.lookup(db).container {
            ItemContainerId::TraitId(_) => {
                let path = names::item_path(&crate_name, &modules, None, &name);
                // alloc: an error
                return Err(refuse(
                    caller,
                    format!("a call of the trait method `{}`", path.display),
                    why::TRAITS,
                ));
            }
            ItemContainerId::ExternBlockId(_) => {
                let path = names::item_path(&crate_name, &modules, None, &name);
                // alloc: an error
                return Err(refuse(
                    caller,
                    format!("a call of the extern function `{}`", path.display),
                    why::UNSAFE,
                ));
            }
            _ if local => {
                let path = names::item_path(&crate_name, &modules, None, &name);
                return Err(refuse(
                    caller,
                    // alloc: an error
                    format!(
                        "a call of `{}`, a function this walk does not translate (one declared in a block, or of a refused impl)",
                        path.display
                    ),
                    why::ITEMS,
                ));
            }
            ItemContainerId::ModuleId(_) => {
                (names::item_path(&crate_name, &modules, None, &name), FunSource::Normal)
            }
            ItemContainerId::ImplId(imp) => {
                let impl_of = inherent_impl(self, &crate_name, &modules, imp).map_err(|err| {
                    // alloc: an error
                    refuse(
                        caller,
                        format!("a call of `{name}` from `{}`: {}", err.function, err.construct),
                        err.why,
                    )
                })?;
                let Some(impl_of) = impl_of else {
                    // An impl with the function in it has items.
                    // alloc: an error
                    return Err(refuse(
                        caller,
                        format!("a call of `{name}`, whose impl lists no items"),
                        why::PHASE,
                    ));
                };
                let path = names::item_path(&crate_name, &modules, Some(&impl_of), &name);
                (path, FunSource::InherentImpl { self_ty: impl_of.self_ty })
            }
        };
        let Signature { generics, sig } = signatures::signature(self, func, &path.display)
            .map_err(|refused| {
                let first = &refused[0];
                refuse(
                    caller,
                    // alloc: an error
                    format!(
                        "a call of `{}`, whose signature has {}",
                        path.display, first.construct
                    ),
                    first.why,
                )
            })?;
        let id = self.translated.fun_decls.reserve_slot();
        self.funs.insert(func, id);
        self.regions.insert(id, generics.regions.len());
        let decl = FunDecl {
            def_id: id,
            item_meta: ItemMeta::dummy_public(
                Span::dummy(),
                path.name,
                false,
                ItemOpacity::Foreign,
            ),
            generics,
            signature: Box::new(sig), // alloc: a Charon signature
            src,
            body: Body::Opaque,
        };
        self.translated.set_new_item_slot(ItemId::Fun(id), ItemByVal::Fun(decl));
        Ok(id)
    }

    /// The number of region parameters of the function `id`: what a call of it passes.
    pub(crate) fn region_count(&self, id: FunDeclId) -> usize {
        self.regions.get(&id).copied().unwrap_or(0)
    }

    /// The intrinsic charon-driver calls for an overflowing operation: `fn
    /// core::intrinsics::<op>_with_overflow<T>(x: T, y: T) -> (T, bool)`, declared on first use.
    pub(crate) fn overflowing(&mut self, op: Overflowing) -> FunDeclId {
        let slot = op as usize;
        if let Some(id) = self.overflowing[slot] {
            return id;
        }
        let name = op.intrinsic();
        let path = names::plain_path(&["core", "intrinsics", name]);
        let t = CTyKind::TypeVar(DeBruijnVar::new_at_zero(TypeVarId::ZERO)).into_ty();
        let mut generics = CGenericParams::empty();
        // rustc's declaration also bounds `T: Copy`; the passes never read the bound, and
        // the Aeneas preset declares no trait to name it by.
        generics.types.push(TypeParam::new(TypeVarId::ZERO, "T".to_owned(), Variance::Unknown)); // alloc: a name
        let mut pair = IndexVec::new(); // alloc: a Charon type's arguments
        pair.push(t.clone()); // clone: a hash-consed handle
        pair.push(CTy::mk_bool());
        let sig = FunSig {
            is_unsafe: false,
            abi: charon_lib::ast::Abi::Rust,
            is_variadic: false,
            inputs: vec![t.clone(), t], // alloc: a Charon signature; clone: a hash-consed handle
            output: types::tuple(pair),
        };
        let id = self.translated.fun_decls.reserve_slot();
        let decl = FunDecl {
            def_id: id,
            item_meta: ItemMeta::dummy_public(
                Span::dummy(),
                path.name,
                false,
                ItemOpacity::Foreign,
            ),
            generics,
            signature: Box::new(sig), // alloc: a Charon signature
            src: FunSource::Normal,
            body: Body::Intrinsic {
                name: name.to_owned(),                                       // alloc: a name
                arg_names: vec![Some("x".to_owned()), Some("y".to_owned())], // alloc: names
                fallback: None,
                fallback_is_spec: false,
            },
        };
        self.translated.set_new_item_slot(ItemId::Fun(id), ItemByVal::Fun(decl));
        self.overflowing[slot] = Some(id);
        self.regions.insert(id, 0);
        id
    }
}

/// The target, as charon-driver's `register_target_info` gives it: the tuple, and what the
/// data layout says. The arch, vendor, OS and environment are the crate's `cfg(target_*)`
/// values where it has them, else the tuple's parts.
fn target_info(
    db: &Db,
    krate: Crate,
    layout: &rustc_abi::TargetDataLayout,
) -> Result<(String, TargetInfo), String> {
    let triple = match &krate.workspace_data(db).target {
        Ok(target) => target.tuple.as_str().to_owned(), // alloc: the target's name, once
        Err(err) => return Err(format!("the target tuple ({err})")), // alloc: an error
    };
    let cfg = krate.cfg_options(db);
    let mut parts = triple.split('-');
    let tuple_arch = parts.next().unwrap_or("");
    let tuple_vendor = parts.next().unwrap_or("unknown");
    let tuple_os = parts.next().unwrap_or("none");
    let tuple_env = parts.next().unwrap_or("");
    let value = |key: &str, fallback: &str| -> String {
        cfg.get_cfg_values(key).next().map_or(fallback, |it| it.as_str()).to_owned() // alloc: a name
    };
    let arch = value("target_arch", tuple_arch);
    let vendor = value("target_vendor", tuple_vendor);
    let os = value("target_os", if tuple_os == "darwin" { "macos" } else { tuple_os });
    let env = value("target_env", tuple_env);

    let mut primitive_alignments = SeqHashMap::default();
    primitive_alignments.insert(ScalarTy::Bool, layout.i8_align.bytes());
    let pointer_align = layout.pointer_align().abi.bytes();
    for (ty, alignment) in [
        (IntegerTy::Signed(CIntTy::I8), layout.i8_align.bytes()),
        (IntegerTy::Signed(CIntTy::I16), layout.i16_align.bytes()),
        (IntegerTy::Signed(CIntTy::I32), layout.i32_align.bytes()),
        (IntegerTy::Signed(CIntTy::I64), layout.i64_align.bytes()),
        (IntegerTy::Signed(CIntTy::I128), layout.i128_align.bytes()),
        (IntegerTy::Signed(CIntTy::Isize), pointer_align),
        (IntegerTy::Unsigned(CUIntTy::U8), layout.i8_align.bytes()),
        (IntegerTy::Unsigned(CUIntTy::U16), layout.i16_align.bytes()),
        (IntegerTy::Unsigned(CUIntTy::U32), layout.i32_align.bytes()),
        (IntegerTy::Unsigned(CUIntTy::U64), layout.i64_align.bytes()),
        (IntegerTy::Unsigned(CUIntTy::U128), layout.i128_align.bytes()),
        (IntegerTy::Unsigned(CUIntTy::Usize), pointer_align),
    ] {
        primitive_alignments.insert(ScalarTy::Integer(ty), alignment);
    }
    primitive_alignments.insert(ScalarTy::Float(FloatTy::F16), layout.f16_align.bytes());
    primitive_alignments.insert(ScalarTy::Float(FloatTy::F32), layout.f32_align.bytes());
    primitive_alignments.insert(ScalarTy::Float(FloatTy::F64), layout.f64_align.bytes());
    primitive_alignments.insert(ScalarTy::Float(FloatTy::F128), layout.f128_align.bytes());
    primitive_alignments.insert(ScalarTy::Char, layout.i32_align.bytes());

    let c_enum_smallest_repr_ty = match layout.c_enum_min_size {
        rustc_abi::Integer::I8 => CIntTy::I8,
        rustc_abi::Integer::I16 => CIntTy::I16,
        rustc_abi::Integer::I32 => CIntTy::I32,
        rustc_abi::Integer::I64 => CIntTy::I64,
        rustc_abi::Integer::I128 => CIntTy::I128,
    };
    let os = match os.as_str() {
        "linux" => Os::Linux,
        "macos" => Os::MacOs,
        "none" => Os::None,
        "unknown" => Os::Unknown,
        other => Os::Other(ustr::Ustr::from(other)),
    };
    let binary_format = match os {
        Os::MacOs => BinaryFormat::MachO,
        Os::Other(name) if name.as_str() == "windows" => BinaryFormat::Coff,
        _ if arch.starts_with("wasm") => BinaryFormat::Wasm,
        _ => BinaryFormat::Elf,
    };
    let info = TargetInfo {
        arch: match arch.as_str() {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::AArch64,
            other => Arch::Other(ustr::Ustr::from(other)),
        },
        vendor,
        os,
        env: match env.as_str() {
            "" => Env::Unspecified,
            "gnu" => Env::Gnu,
            other => Env::Other(ustr::Ustr::from(other)),
        },
        cfg_abi: CfgAbi::Unspecified,
        binary_format,
        target_pointer_size: layout.pointer_size().bytes(),
        is_little_endian: matches!(layout.endian, rustc_abi::Endian::Little),
        c_enum_smallest_repr_ty,
        primitive_alignments,
    };
    Ok((triple, info))
}

impl Ctx<'_> {
    /// The local struct or enum declared as `id`.
    pub(crate) fn adt_of(&self, id: TypeDeclId) -> Option<AdtId> {
        self.adts.iter().find_map(|(&adt, &it)| (it == id).then_some(adt))
    }
}

/// The metadata of an item of the crate being translated. `started_from` makes it a root of
/// Charon's reachability walk (`reorder_decls`), which drops every item no root reaches.
pub(crate) fn local_meta(name: charon_lib::ast::Name) -> ItemMeta {
    let mut meta = ItemMeta::dummy_public(Span::dummy(), name, true, ItemOpacity::Transparent);
    meta.started_from = true;
    meta
}
