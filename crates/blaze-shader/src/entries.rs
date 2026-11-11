//! A crate's entry points, read from its `GPU_ENTRIES` by const evaluation: the same constants the
//! host binds through, so the two can't number a slot differently (`mirrors-derive-or-check`).
//!
//! The gpu crate's descriptor types (crates/gpu/src/entry.rs) are mirrored here as owned data.
//! Nothing is decoded by assumed offsets: each field is found by its name in the type's
//! declaration and read at the offset the front end's layout gives it, an enum's variant by the
//! layout's discriminant and matched by its name. A name the mirror does not know is an error
//! naming it, so a change to the gpu crate that this side has not followed stops the build (the
//! check half of the rule) instead of misreading bytes.
use base_db::{Crate, Db};
use hir_def::{
    AdtId, EnumId, FunctionId, ModuleDefId, StaticId, VariantId, nameres::crate_def_map,
};
use hir_expand::name::Name;
use hir_ty::{
    MemoryMap, ParamEnvAndCrate,
    db::HirDatabase as _,
    next_solver::{DbInterner, ParamEnv, Ty, TyKind},
    traits::StoredParamEnvAndCrate,
    layout::Layout,
    utils::detect_variant_from_bytes,
};
use intern::Symbol;
use rustc_abi::{Endian, TargetDataLayout};
use rustc_type_ir::inherent::{AdtDef as _, GenericArgs as _, IntoKind as _};

use crate::refuse::{Error, ty_name};

/// The static a device build roots at, at the crate root (docs/future/gpu/shaders.md).
pub const ENTRIES: &str = "GPU_ENTRIES";

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    Vertex,
    Fragment,
    /// A kernel, its workgroup `x * y * z` threads.
    Compute { x: u32, y: u32, z: u32 },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ValueTy {
    F32,
    Vec2,
    Vec3,
    Vec4,
    U32,
    UVec4,
    I32,
    IVec2,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Interp {
    Perspective,
    Linear,
    Flat,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum FieldKind {
    Position,
    Value { ty: ValueTy, interp: Interp },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Field {
    pub name: String,
    pub kind: FieldKind,
    pub location: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    VertexIndex,
    InstanceIndex,
    Varyings(Vec<Field>),
    Uniform { size: u32 },
    Storage { stride: u32 },
    Texture2d,
    Sampler,
    Thread,
    RawOut { stride: u32 },
    Tlas,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Out {
    Varyings(Vec<Field>),
    Color,
    Nothing,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Table {
    None,
    Buffer,
    Texture,
    Sampler,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Slot {
    pub table: Table,
    pub index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Param {
    pub name: String,
    pub kind: Kind,
    pub slot: Slot,
}

/// One entry point: its descriptor and the function `code` names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub name: String,
    pub stage: Stage,
    pub params: Vec<Param>,
    pub out: Out,
    /// Where the sizes of the storage buffers are bound (`gpu::sizes_slot`, read from the
    /// descriptor like every other slot); `None` when the entry binds no storage buffer.
    pub sizes: Option<Slot>,
    pub func: FunctionId,
}

/// The entries of `krate`'s `GPU_ENTRIES`, in their order there.
pub fn entries(db: &Db, krate: Crate) -> Result<Vec<Entry>, Error> {
    let krate_name = crate_name(db, krate);
    let no = |why: String| Error::NoEntries { krate: krate_name.clone(), why }; // clone: an error path
    let def_map = crate_def_map(db, krate);
    let root = def_map.crate_root(db);
    let found = def_map.module(db, root).scope(db).get(&Name::new_symbol_root(Symbol::intern(ENTRIES)));
    let Some(ModuleDefId::StaticId(statik)) = found.values.map(|it| it.def) else {
        // alloc: an error
        return Err(no(format!(
            "the crate root has no static `{ENTRIES}`: name the crate's entry points with \
             `gpu::entries![..]` there (crates/gpu/src/lib.rs)"
        )));
    };
    let reader = Reader::new(db, krate, statik).map_err(&no)?;
    reader.entries().map_err(no)
}

/// A crate's name, as its display name gives it.
pub(crate) fn crate_name(db: &Db, krate: Crate) -> String {
    match &krate.extra_data(db).display_name {
        Some(name) => name.crate_name().as_str().to_owned(), // alloc: a name
        None => "crate".to_owned(),                          // alloc: a name
    }
}

/// A value read out of the evaluated static: its type and its bytes.
#[derive(Copy, Clone)]
struct Value<'a, 'db> {
    ty: Ty<'db>,
    bytes: &'a [u8],
}

struct Reader<'db> {
    db: &'db Db,
    krate: Crate,
    interner: DbInterner<'db>,
    env: StoredParamEnvAndCrate,
    dl: &'db TargetDataLayout,
    ty: Ty<'db>,
    memory: Box<[u8]>,
    map: MemoryMap<'db>,
}

impl<'db> Reader<'db> {
    /// `ty` as an error names it.
    fn shown(&self, ty: Ty<'db>) -> String {
        ty_name(self.db, self.krate, ty)
    }

    /// The bytes of a value of `ty` are fewer than its layout reads.
    fn short(&self, ty: Ty<'db>) -> String {
        format!("the bytes of a `{}` are cut short", self.shown(ty)) // alloc: an error
    }

    fn new(db: &'db Db, krate: Crate, statik: StaticId) -> Result<Reader<'db>, String> {
        let allocation = db
            .const_eval_static(statik)
            .map_err(|e| format!("`{ENTRIES}` did not evaluate: {e:?}"))?; // alloc: an error
        let dl = db
            .target_data_layout(krate)
            .map_err(|e| format!("the device target's data layout: {e:?}"))?; // alloc: an error
        let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
        Ok(Reader {
            db,
            krate,
            interner: DbInterner::new_with(db, krate),
            env,
            dl,
            ty: allocation.ty,
            memory: allocation.memory.clone(),   // clone: the static's bytes, read once
            map: allocation.memory_map.clone(), // clone: its memory, read once
        })
    }

    fn entries(&self) -> Result<Vec<Entry>, String> {
        let root = Value { ty: self.ty, bytes: &self.memory };
        self.expect_adt(root, "gpu::entry::Entries")?;
        let list = self.field(root, "0")?;
        let mut out = Vec::new(); // alloc: the entries, once per device build
        for entry in self.slice(list)? {
            let desc = self.deref(self.field(entry, "desc")?)?;
            let code = self.field(self.field(entry, "code")?, "0")?;
            out.push(self.entry(desc, code)?);
        }
        Ok(out)
    }

    fn entry(&self, desc: Value<'_, 'db>, code: Value<'_, 'db>) -> Result<Entry, String> {
        let name = self.str(self.field(desc, "name")?)?;
        let (stage, axes) = self.variant(self.field(desc, "stage")?)?;
        let stage = match stage.as_str() {
            "Vertex" => Stage::Vertex,
            "Fragment" => Stage::Fragment,
            "Compute" => {
                let [x, y, z] = axes[..] else {
                    return Err("`Stage::Compute` has other than three fields".to_owned()); // alloc: an error
                };
                Stage::Compute { x: self.u32(x)?, y: self.u32(y)?, z: self.u32(z)? }
            }
            other => return Err(unknown("Stage", other)),
        };
        let mut params = Vec::new(); // alloc: an entry's parameters
        for param in self.slice(self.field(desc, "params")?)? {
            params.push(self.param(param)?);
        }
        let out = {
            let (variant, fields) = self.variant(self.field(desc, "out")?)?;
            match variant.as_str() {
                "Varyings" => Out::Varyings(self.fields(one(&fields, "Out::Varyings")?)?),
                "Color" => Out::Color,
                "Nothing" => Out::Nothing,
                other => return Err(unknown("Out", other)),
            }
        };
        let func = self.function(code, &name)?;
        let sizes = {
            let (variant, payload) = self.variant(self.field(desc, "sizes")?)?;
            match variant.as_str() {
                "None" => None,
                "Some" => Some(self.slot(one(&payload, "Option::Some")?)?),
                other => return Err(unknown("Option", other)),
            }
        };
        Ok(Entry { name, stage, params, out, sizes, func })
    }

    fn param(&self, param: Value<'_, 'db>) -> Result<Param, String> {
        let name = self.str(self.field(param, "name")?)?;
        let (variant, fields) = self.variant(self.field(param, "kind")?)?;
        let kind = match variant.as_str() {
            "VertexIndex" => Kind::VertexIndex,
            "InstanceIndex" => Kind::InstanceIndex,
            "Varyings" => Kind::Varyings(self.fields(one(&fields, "Kind::Varyings")?)?),
            "Uniform" => Kind::Uniform { size: self.u32(one(&fields, "Kind::Uniform")?)? },
            "Storage" => Kind::Storage { stride: self.u32(one(&fields, "Kind::Storage")?)? },
            "Texture2d" => Kind::Texture2d,
            "Sampler" => Kind::Sampler,
            "Thread" => Kind::Thread,
            "RawOut" => Kind::RawOut { stride: self.u32(one(&fields, "Kind::RawOut")?)? },
            "Tlas" => Kind::Tlas,
            other => return Err(unknown("Kind", other)),
        };
        let slot = self.slot(self.field(param, "slot")?)?;
        Ok(Param { name, kind, slot })
    }

    fn slot(&self, slot: Value<'_, 'db>) -> Result<Slot, String> {
        let table = match self.variant(self.field(slot, "table")?)?.0.as_str() {
            "None" => Table::None,
            "Buffer" => Table::Buffer,
            "Texture" => Table::Texture,
            "Sampler" => Table::Sampler,
            other => return Err(unknown("Table", other)),
        };
        let index = self.u32(self.field(slot, "index")?)?;
        Ok(Slot { table, index })
    }

    fn fields(&self, list: Value<'_, 'db>) -> Result<Vec<Field>, String> {
        let mut out = Vec::new(); // alloc: a varyings struct's fields
        for field in self.slice(list)? {
            let name = self.str(self.field(field, "name")?)?;
            let (variant, payload) = self.variant(self.field(field, "kind")?)?;
            let kind = match variant.as_str() {
                "Position" => FieldKind::Position,
                "Value" => {
                    let [ty, interp] = payload[..] else {
                        return Err("`FieldKind::Value` has other than two fields".to_owned()); // alloc: an error
                    };
                    let ty = match self.variant(ty)?.0.as_str() {
                        "F32" => ValueTy::F32,
                        "Vec2" => ValueTy::Vec2,
                        "Vec3" => ValueTy::Vec3,
                        "Vec4" => ValueTy::Vec4,
                        "U32" => ValueTy::U32,
                        "UVec4" => ValueTy::UVec4,
                        "I32" => ValueTy::I32,
                        "IVec2" => ValueTy::IVec2,
                        other => return Err(unknown("ValueTy", other)),
                    };
                    let interp = match self.variant(interp)?.0.as_str() {
                        "Perspective" => Interp::Perspective,
                        "Linear" => Interp::Linear,
                        "Flat" => Interp::Flat,
                        other => return Err(unknown("Interp", other)),
                    };
                    FieldKind::Value { ty, interp }
                }
                other => return Err(unknown("FieldKind", other)),
            };
            let location = {
                let (variant, payload) = self.variant(self.field(field, "location")?)?;
                match variant.as_str() {
                    "None" => None,
                    "Some" => Some(self.u32(one(&payload, "Option::Some")?)?),
                    other => return Err(unknown("Option", other)),
                }
            };
            out.push(Field { name, kind, location });
        }
        Ok(out)
    }

    /// The function a `Code`'s pointer names.
    fn function(&self, code: Value<'_, 'db>, entry: &str) -> Result<FunctionId, String> {
        let word = self.word(code.bytes)?;
        match self.map.function_of(word as usize).map(|ty| ty.kind()) {
            Some(TyKind::FnDef(def, args)) => match def.0 {
                hir_def::CallableDefId::FunctionId(func) if args.skip_binder().is_empty() => Ok(func),
                hir_def::CallableDefId::FunctionId(_) => {
                    // alloc: an error
                    Err(format!("the code of entry `{entry}` is a generic function: an entry is not generic"))
                }
                _ => Err(format!("the code of entry `{entry}` is not a function")), // alloc: an error
            },
            _ => Err(format!("the code of entry `{entry}` is not a function pointer the evaluation made")), // alloc: an error
        }
    }

    /// `f` of the front end's layout of `ty`, the one native codegen lays it out by.
    fn with_layout<R>(&self, ty: Ty<'db>, f: impl FnOnce(&Layout) -> R) -> Result<R, String> {
        let layout = self
            .db
            .layout_of_ty(ty.store(), self.env.clone()) // clone: a stored handle
            .map_err(|e| format!("the layout of `{ty}`: {e:?}", ty = self.shown(ty)))?; // alloc: an error
        Ok(f(&layout))
    }

    fn size(&self, ty: Ty<'db>) -> Result<usize, String> {
        self.with_layout(ty, |l| l.size.bytes() as usize)
    }

    fn expect_adt(&self, value: Value<'_, 'db>, path: &str) -> Result<(), String> {
        let found = match value.ty.kind() {
            TyKind::Adt(def, _) => blaze_items::key::path(self.db, blaze_items::ItemId::Adt(def.def_id())),
            _ => self.shown(value.ty),
        };
        if found != path {
            // alloc: an error
            return Err(format!("`{ENTRIES}` is a `{found}`, not the gpu crate's `{path}` (write it with `gpu::entries!`)"));
        }
        Ok(())
    }

    /// The field `name` of a struct value (a tuple struct's fields are named `0`, `1`, ...).
    fn field<'a>(&self, value: Value<'a, 'db>, name: &str) -> Result<Value<'a, 'db>, String> {
        let TyKind::Adt(def, args) = value.ty.kind() else {
            return Err(format!("`{}` has no field `{name}`: it is not a struct", self.shown(value.ty))); // alloc: an error
        };
        let AdtId::StructId(id) = def.def_id() else {
            return Err(format!("`{}` has no field `{name}`: it is not a struct", self.shown(value.ty))); // alloc: an error
        };
        let variant = VariantId::StructId(id);
        let Some(local) = variant.fields(self.db).field(&Name::new_symbol_root(Symbol::intern(name))) else {
            // alloc: an error
            return Err(format!("`{}` has no field `{name}`: the gpu crate's descriptors changed", self.shown(value.ty)));
        };
        let index = local.into_raw().into_u32() as usize;
        let ty = self.field_ty(variant, index, args)?;
        let at = self.with_layout(value.ty, |l| l.fields.offset(index).bytes() as usize)?;
        let size = self.size(ty)?;
        let bytes = value.bytes.get(at..at + size).ok_or_else(|| self.short(value.ty))?;
        Ok(Value { ty, bytes })
    }

    fn field_ty(
        &self,
        variant: VariantId,
        index: usize,
        args: hir_ty::next_solver::GenericArgs<'db>,
    ) -> Result<Ty<'db>, String> {
        let (_, field) = self
            .db
            .field_types(variant)
            .iter()
            .nth(index)
            .ok_or_else(|| format!("field {index} of `{variant:?}`"))?; // alloc: an error
        Ok(field.ty().instantiate(self.interner, args).skip_norm_wip())
    }

    /// An enum value's variant, by name, with its fields' values.
    fn variant<'a>(&self, value: Value<'a, 'db>) -> Result<(String, Vec<Value<'a, 'db>>), String> {
        let TyKind::Adt(def, args) = value.ty.kind() else {
            return Err(format!("`{}` is not an enum", self.shown(value.ty))); // alloc: an error
        };
        let AdtId::EnumId(e) = def.def_id() else {
            return Err(format!("`{}` is not an enum", self.shown(value.ty))); // alloc: an error
        };
        let (db, dl, bytes) = (self.db, self.dl, value.bytes);
        let found = self.with_layout(value.ty, |layout| {
            detect_variant_from_bytes(layout, db, dl, bytes, e).map(|(variant, variant_layout)| {
                let offsets: Vec<usize> = (0..variant_layout.fields.count()) // alloc: a variant's offsets
                    .map(|i| variant_layout.fields.offset(i).bytes() as usize)
                    .collect();
                (variant, offsets)
            })
        })?;
        let (variant, offsets) =
            found.ok_or_else(|| format!("no variant of `{}` has these bytes", self.shown(value.ty)))?; // alloc: an error
        let name = variant_name(self.db, e, variant)?;
        let mut fields = Vec::new(); // alloc: a variant's fields
        let id = VariantId::EnumVariantId(variant);
        for (index, at) in offsets.into_iter().enumerate() {
            let ty = self.field_ty(id, index, args)?;
            let size = self.size(ty)?;
            let bytes = value.bytes.get(at..at + size).ok_or_else(|| self.short(value.ty))?;
            fields.push(Value { ty, bytes });
        }
        Ok((name, fields))
    }

    fn word(&self, bytes: &[u8]) -> Result<u64, String> {
        let size = self.dl.pointer_size().bytes() as usize;
        let word = bytes.get(..size).ok_or_else(|| "a pointer cut short".to_owned())?; // alloc: an error
        let mut out = [0u8; 8];
        match self.dl.endian {
            Endian::Little => out[..size].copy_from_slice(word),
            Endian::Big => {
                for (i, b) in word.iter().rev().enumerate() {
                    out[i] = *b;
                }
            }
        }
        Ok(u64::from_le_bytes(out))
    }

    fn u32(&self, value: Value<'_, 'db>) -> Result<u32, String> {
        let bytes: [u8; 4] = value.bytes.try_into().map_err(|_| self.short(value.ty))?;
        Ok(match self.dl.endian {
            Endian::Little => u32::from_le_bytes(bytes),
            Endian::Big => u32::from_be_bytes(bytes),
        })
    }

    /// What a thin reference points to.
    fn deref<'a>(&'a self, value: Value<'_, 'db>) -> Result<Value<'a, 'db>, String> {
        let TyKind::Ref(_, pointee, _) = value.ty.kind() else {
            return Err(format!("`{}` is not a reference", self.shown(value.ty))); // alloc: an error
        };
        let addr = self.word(value.bytes)? as usize;
        let size = self.size(pointee)?;
        let bytes = self.memory_at(addr, size, pointee)?;
        Ok(Value { ty: pointee, bytes })
    }

    /// The elements of a `&[T]`.
    fn slice<'a>(&'a self, value: Value<'_, 'db>) -> Result<Vec<Value<'a, 'db>>, String> {
        let TyKind::Ref(_, pointee, _) = value.ty.kind() else {
            return Err(format!("`{}` is not a slice reference", self.shown(value.ty))); // alloc: an error
        };
        let TyKind::Slice(elem) = pointee.kind() else {
            return Err(format!("`{}` is not a slice reference", self.shown(value.ty))); // alloc: an error
        };
        let ptr = self.dl.pointer_size().bytes() as usize;
        let addr = self.word(value.bytes)? as usize;
        let len = self.word(value.bytes.get(ptr..).ok_or_else(|| self.short(value.ty))?)? as usize;
        let stride = self.size(elem)?;
        let bytes = self.memory_at(addr, len * stride, pointee)?;
        Ok((0..len).map(|i| Value { ty: elem, bytes: &bytes[i * stride..(i + 1) * stride] }).collect())
    }

    fn str(&self, value: Value<'_, 'db>) -> Result<String, String> {
        let TyKind::Ref(_, pointee, _) = value.ty.kind() else {
            return Err(format!("`{}` is not a `&str`", self.shown(value.ty))); // alloc: an error
        };
        if !matches!(pointee.kind(), TyKind::Str) {
            return Err(format!("`{}` is not a `&str`", self.shown(value.ty))); // alloc: an error
        }
        let ptr = self.dl.pointer_size().bytes() as usize;
        let addr = self.word(value.bytes)? as usize;
        let len = self.word(value.bytes.get(ptr..).ok_or_else(|| self.short(value.ty))?)? as usize;
        let bytes = self.memory_at(addr, len, pointee)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "a name that is not UTF-8".to_owned()) // alloc: a name
    }

    fn memory_at(&self, addr: usize, size: usize, ty: Ty<'db>) -> Result<&[u8], String> {
        if size == 0 {
            return Ok(&[]);
        }
        self.map
            .get(addr, size)
            .ok_or_else(|| format!("the memory of a `{ty}` at {addr:#x} is not in the evaluated static", ty = self.shown(ty))) // alloc: an error
    }
}

fn variant_name(db: &Db, e: EnumId, variant: hir_def::EnumVariantId) -> Result<String, String> {
    e.enum_variants(db)
        .variants
        .iter()
        .find(|(_, (id, _))| *id == variant)
        .map(|(name, _)| name.as_str().to_owned()) // alloc: a name
        // alloc: an error
        .ok_or_else(|| format!("a variant of `{}`", blaze_items::key::path(db, blaze_items::ItemId::Adt(AdtId::EnumId(e)))))
}

fn one<'a, 'db>(fields: &[Value<'a, 'db>], what: &str) -> Result<Value<'a, 'db>, String> {
    match fields {
        [only] => Ok(*only),
        _ => Err(format!("`{what}` has other than one field: the gpu crate's descriptors changed")), // alloc: an error
    }
}

fn unknown(ty: &str, variant: &str) -> String {
    // alloc: an error
    format!(
        "`gpu::{ty}::{variant}` is not known to the device backend: blaze-shader's mirror of \
         crates/gpu/src/entry.rs needs it"
    )
}

