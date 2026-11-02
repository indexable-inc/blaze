//! What the IR builder's requests are as references: one function, static or outside symbol
//! for each, the same whether the IR builder or the walk of the MIR ([`super::refs`]) asks.
//! Equal references are one place, so both go through here.

use std::{
    alloc::{Allocator, Global},
    borrow::Cow,
    cell::{OnceCell, RefCell},
    hash::BuildHasher as _,
};

use blaze_codegen::{Target, backend::FrontRef, template::Refs};
use blaze_mem::Frame;
use cranelift_codegen::ir;
use hashbrown::HashTable;
use blaze_hash::FixedState;
use hir_def::{
    FunctionId, ItemContainerId, Lookup, StaticId,
    attrs::AttrFlags,
    signatures::{FunctionSignature, StaticSignature},
};
use base_db::{Db, Crate};
use hir_ty::{
    db::HirDatabase,
    next_solver::{
        DbInterner, GenericArgs, ParamEnv, TypingMode,
        infer::{DbInternerInferExt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
    traits::StoredParamEnvAndCrate,
};
use rustc_type_ir::{TypeFoldable, TypeVisitableExt};

use super::{
    DataPlace, HashSource, Hashes, HirInstance, HirStatic, Kind, Places,
    convert::{DataTarget, Resolver},
};
use crate::{
    ConstBlob, Instance, InstanceDef, erase_regions,
    fragment::{DataRequest, FuncRequest, FuncTarget, Requests, TextPart},
    home::Homes,
    link_name, static_link_name,
};

pub(crate) type Ref = FrontRef<HirInstance, HirStatic>;

/// The symbol of the routine the unwinder asks about a frame with landing pads.
pub(crate) const PERSONALITY: &str = crate::unwind::PERSONALITY;

/// The reference a function request of the IR builder is.
pub(crate) fn func_ref(
    db: &Db,
    homes: &mut Homes,
    target: &FuncTarget,
) -> Result<Ref, String> {
    match target {
        // clone: `FrontRef::Extern` owns its name; `place_requests` places a symbol by `&str`
        FuncTarget::Symbol(symbol) => Ok(FrontRef::Extern(symbol.clone())),
        FuncTarget::Path(path) => {
            // alloc: the error's message
            Err(format!("a call of rustc's `{path}`, which a build without rustc has none of"))
        }
        FuncTarget::Instance(instance, env) => instance_ref(db, homes, instance, env),
        // clone: only a call of an `asm!` block's function, whose `Kind::Asm` owns its text
        FuncTarget::Asm(text) => Ok(FrontRef::Function(HirInstance(Kind::Asm(text.clone(), false)))),
    }
}

/// The reference a call of `def` with `args` in `env` is: the instance, compiled in the crate
/// every asker agrees on, or the symbol of a function that has no body of Rust.
pub(crate) fn call_ref<'db>(
    db: &'db Db,
    homes: &mut Homes,
    def: InstanceDef,
    args: GenericArgs<'db>,
    env: &StoredParamEnvAndCrate,
) -> Result<Ref, String> {
    let interner = DbInterner::new_with(db, env.krate);
    let instance = Instance { def, args: erase_regions(interner, args).store(), checked: false };
    instance_ref(db, homes, &instance, env)
}

/// As [`call_ref`], of an instance as the IR builder holds it.
pub(crate) fn instance_ref(
    db: &Db,
    homes: &mut Homes,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
) -> Result<Ref, String> {
    let mut instance = Instance { checked: false, ..*instance };
    let args = instance.args.as_ref();
    if args.has_aliases() {
        instance.args = evaluated(db, Some(env.as_ref(db).krate), args)?.store();
    }
    if let InstanceDef::Fn(func) = instance.def
        && !FunctionSignature::of(db, func).has_body()
    {
        return bodyless(db, func, instance.args.as_ref(), env).map(FrontRef::Extern);
    }
    let env = homes.home(db, &instance, env);
    Ok(FrontRef::Function(HirInstance(Kind::Code(instance, env))))
}

/// The symbol a function without a body of Rust is: an item of an `extern` block. A trait's
/// method without a default body, called with `args` in `env`, is an error naming why no
/// impl's method was found ([`crate::no_impl_method`]).
fn bodyless(
    db: &Db,
    func: FunctionId,
    args: GenericArgs<'_>,
    env: &StoredParamEnvAndCrate,
) -> Result<String, String> {
    let name = FunctionSignature::of(db, func).name.as_str();
    match func.lookup(db).container {
        ItemContainerId::ExternBlockId(_) => {
            let Some(symbol) = link_name(db, func) else {
                // alloc: the error's message
                return Err(format!("`{name}` of an `extern` block has no link name"));
            };
            if symbol.starts_with("llvm.") {
                // alloc: the error's message
                return Err(format!("a call of LLVM's intrinsic `{symbol}` the IR builder left"));
            }
            Ok(symbol)
        }
        ItemContainerId::TraitId(_) => match db.lookup_impl_method_or_why(env.as_ref(db), func, args) {
            Err(why) => Err(crate::no_impl_method(db, func, args, why)),
            // alloc: the error's message
            Ok(_) => Err(format!("a call of the trait method `{name}`, not of the impl's method it resolves to")),
        },
        _ if FunctionSignature::is_intrinsic(db, func) => {
            // alloc: the error's message
            Err(format!("a call of the intrinsic `{name}`, which has no fallback body"))
        }
        _ => Err(format!("a call of `{name}`, which has no body")), // alloc: the error's message
    }
}

/// The reference a static is: an item of an `extern` block by its symbol, any other by
/// which static it is. An `extern_weak` one is a static of blaze's that holds the address of
/// its symbol, 0 when nothing defines it ([`weak_symbol`]).
pub(crate) fn static_ref(db: &Db, id: StaticId) -> Result<Ref, String> {
    if weak_symbol(db, id)?.is_some() {
        return Ok(FrontRef::Static(HirStatic(id)));
    }
    match static_link_name(db, id) {
        Some(symbol) => Ok(FrontRef::Extern(symbol)),
        None => Ok(FrontRef::Static(HirStatic(id))),
    }
}

/// The symbol of an `extern_weak` static of an `extern` block.
pub(crate) fn weak_symbol(db: &Db, id: StaticId) -> Result<Option<String>, String> {
    if !AttrFlags::linkage(db, id.into()).is_some_and(|it| it.as_str() == "extern_weak") {
        return Ok(None);
    }
    match static_link_name(db, id) {
        Some(symbol) => Ok(Some(symbol)),
        // alloc: the error's message
        None => Err(format!(
            "`{}` is `extern_weak` but of no `extern` block",
            StaticSignature::of(db, id).name.as_str()
        )),
    }
}

/// The reference a routine Cranelift calls itself is: the C library's.
pub(crate) fn libcall_ref(libcall: ir::LibCall) -> Ref {
    FrontRef::Extern(libcall_symbol(libcall))
}

pub(crate) fn libcall_symbol(libcall: ir::LibCall) -> String {
    cranelift_module::default_libcall_names()(libcall)
}

/// The C library's name of each routine Cranelift calls itself, made once a process:
/// `default_libcall_names` makes a boxed closure and a `String` a call.
pub(crate) fn libcall_name(libcall: ir::LibCall) -> Result<&'static str, String> {
    static NAMES: std::sync::LazyLock<Box<[(ir::LibCall, String)]>> = std::sync::LazyLock::new(|| {
        let name = cranelift_module::default_libcall_names();
        // alloc: once a process
        ir::LibCall::all_libcalls().iter().map(|&it| (it, name(it))).collect()
    });
    match NAMES.iter().find(|it| it.0 == libcall) {
        Some((_, name)) => Ok(name.as_str()),
        // alloc: an error
        None => Err(format!("{libcall:?}, which Cranelift's list of its libcalls lacks")),
    }
}

/// The reference of the personality routine of a function with landing pads.
pub(crate) fn personality_ref() -> Ref {
    // alloc: once per function with landing pads; `FrontRef::Extern` owns its name
    FrontRef::Extern(PERSONALITY.to_owned())
}
/// The places of a list of references: a function's, which the walk made and nothing is
/// added to, or a static's, made as its requests come.
pub(crate) struct Placer<'a, A: Allocator = Global> {
    /// A function's: the walk's list as the database memoizes it, borrowed.
    refs: Cow<'a, [Ref]>,
    /// Each place, found by its reference's hash and checked against `refs`, so no reference is
    /// cloned into a key: 4 bytes and a control byte a place, one probe a lookup. In `A`: a
    /// unit's frame when it is a function's.
    index: HashTable<u32, A>,
    open: bool,
}

/// A reference's hash: a symbol's is of its name alone, so [`Placer::symbol`] finds one
/// by a `&str` without making the reference.
fn hash(it: &Ref) -> u64 {
    match it {
        FrontRef::Extern(name) => symbol_hash(name),
        other => FixedState.hash_one(other),
    }
}

fn symbol_hash(name: &str) -> u64 {
    FixedState.hash_one(name)
}

impl<'a> Placer<'a> {
    pub(crate) fn listed(refs: impl Into<Cow<'a, [Ref]>>) -> Placer<'a> {
        Placer::listed_in(refs, Global)
    }

    pub(crate) fn open() -> Placer<'a> {
        // alloc: empty, allocates nothing until a static's first reference
        Placer { refs: Cow::Owned(Vec::new()), index: HashTable::new(), open: true }
    }
}

impl<'a, A: Allocator> Placer<'a, A> {
    /// [`Placer::listed`] with its index in `alloc`: a unit's frame.
    pub(crate) fn listed_in(refs: impl Into<Cow<'a, [Ref]>>, alloc: A) -> Placer<'a, A> {
        let refs = refs.into();
        let mut index = HashTable::with_capacity_in(refs.len(), alloc);
        for (place, it) in refs.iter().enumerate() {
            let hashed = hash(it);
            if index.find(hashed, |&at: &u32| refs[at as usize] == *it).is_none() {
                index.insert_unique(hashed, place as u32, |&at| hash(&refs[at as usize]));
            }
        }
        Placer { refs, index, open: false }
    }

    pub(crate) fn place(&mut self, it: Ref) -> Result<u32, String> {
        // alloc: the error's message
        self.place_named(it, |it, _| format!("{it:?}"))
    }

    /// [`Self::place`], an error naming the reference by `name`.
    fn place_named(&mut self, it: Ref, name: impl FnOnce(&Ref, &[Ref]) -> String) -> Result<u32, String> {
        let hashed = hash(&it);
        let refs = &self.refs;
        if let Some(&place) = self.index.find(hashed, |&at| refs[at as usize] == it) {
            return Ok(place);
        }
        if !self.open {
            // alloc: the error's message
            return Err(format!("the IR asks for {}, which its references do not list", name(&it, refs)));
        }
        self.add(hashed, it)
    }

    /// The place of the symbol `name`, which is made a reference only when it is new.
    pub(crate) fn symbol(&mut self, name: &str) -> Result<u32, String> {
        let hashed = symbol_hash(name);
        let refs = &self.refs;
        let same = |&at: &u32| matches!(&refs[at as usize], FrontRef::Extern(it) if it == name);
        if let Some(&place) = self.index.find(hashed, same) {
            return Ok(place);
        }
        // alloc: only a symbol not placed yet, whose reference owns its name
        self.add(hashed, FrontRef::Extern(name.to_owned()))
    }

    fn add(&mut self, hashed: u64, it: Ref) -> Result<u32, String> {
        if !self.open {
            // alloc: the error's message
            return Err(format!("the IR asks for {it:?}, which its references do not list"));
        }
        let place = self.refs.len() as u32;
        self.refs.to_mut().push(it);
        let refs = &self.refs;
        self.index.insert_unique(hashed, place, |&at| hash(&refs[at as usize]));
        Ok(place)
    }

    pub(crate) fn refs(&self) -> &[Ref] {
        &self.refs
    }

    pub(crate) fn into_refs(self) -> Vec<Ref> {
        self.refs.into_owned()
    }
}

/// A reference as an error names it: a function by its path and the crate it is compiled in.
fn ref_name(db: &Db, it: &Ref) -> String {
    match it {
        FrontRef::Function(instance) => super::Named(db, instance).to_string(), // alloc: the error's message
        // alloc: the error's message
        FrontRef::Static(HirStatic(id)) => format!("the static `{}`", StaticSignature::of(db, *id).name.as_str()),
        FrontRef::Extern(name) => format!("the symbol `{name}`"), // alloc: the error's message
        FrontRef::Weak(name) => format!("the weak symbol `{name}`"), // alloc: the error's message
    }
}

/// A reference the IR asks for that `refs` lacks, named, with the crates `refs` compiles the
/// same instance in when it has it elsewhere: the walk and the IR builder disagree on its home.
fn unlisted(db: &Db, it: &Ref, refs: &[Ref]) -> String {
    let mut said = ref_name(db, it);
    if let FrontRef::Function(HirInstance(Kind::Code(instance, _))) = it {
        for other in refs {
            if let FrontRef::Function(other @ HirInstance(Kind::Code(same, _))) = other
                && same == instance
            {
                // alloc: the error's message
                said.push_str(&format!("; it lists {}", super::Named(db, other)));
            }
        }
    }
    said
}

/// The place of each of `requests`, whose `TypeId`s it fills in. `krate` is the crate whose
/// code makes them, `None` for assembly.
pub(crate) fn place_requests<A: Allocator>(
    db: &Db,
    homes: &mut Homes,
    hashes: &HashSource,
    krate: Option<Crate>,
    placer: &mut Placer<'_, A>,
    requests: &mut Requests,
) -> Result<Places, String> {
    let mut funcs = Vec::with_capacity(requests.funcs.len());
    for request in &requests.funcs {
        let place = match &request.target {
            FuncTarget::Symbol(symbol) => placer.symbol(symbol)?,
            target => placer.place_named(func_ref(db, homes, target)?, |it, refs| unlisted(db, it, refs))?,
        };
        funcs.push(place);
    }
    let mut datas = Vec::with_capacity(requests.datas.len());
    for request in &mut requests.datas {
        let place = match request {
            DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => {
                fill_type_ids(db, hashes, krate, blob)?;
                DataPlace::Own
            }
            DataRequest::Static(id) => {
                DataPlace::Place(placer.place_named(static_ref(db, *id)?, |it, refs| unlisted(db, it, refs))?)
            }
            DataRequest::Location(Some((owner, span))) => {
                DataPlace::Position(hashes.span(db, *owner, *span)?)
            }
            DataRequest::Location(None) => DataPlace::Nowhere,
            DataRequest::Declaration(func) => DataPlace::Position(hashes.declaration(db, *func)?),
            DataRequest::FilePath(file) => DataPlace::Text(hashes.file_name(db, *file)?),
            DataRequest::Text(parts) => {
                // alloc: the text its data place keeps, one per `file!()` text of the function
                let mut text = String::new();
                for part in parts.iter() {
                    match part {
                        TextPart::Text(it) => text.push_str(it),
                        TextPart::FilePath(file) => text.push_str(&hashes.file_name(db, *file)?),
                    }
                }
                DataPlace::Text(text)
            }
        };
        datas.push(place);
    }
    Ok(Places { funcs, datas, refs: placer.refs().len() as u32 })
}

/// `value` as codegen sees a monomorphized type or constant, which is all [`Hashes`] gives an
/// ID of: every alias normalized and every constant evaluated, in `krate` with no parameters
/// (rustc's `TypingEnv::fully_monomorphized`). Monomorphization normalizes the aliases of a
/// body's types but leaves their constants unevaluated: `[u8; N]` of a `const N`. One with
/// neither, as nearly every one, is given back with nothing made.
pub(crate) fn evaluated<'db, T>(
    db: &'db Db,
    krate: Option<Crate>,
    value: T,
) -> Result<T, String>
where
    T: TypeFoldable<DbInterner<'db>> + std::fmt::Debug + Copy,
{
    if !value.has_aliases() {
        return Ok(value);
    }
    let Some(krate) = krate else {
        // alloc: the error's message
        return Err(format!("`{value:?}` has aliases left, and no crate to normalize them in"));
    };
    let infcx = DbInterner::new_with(db, krate).infer_ctxt().build(TypingMode::PostAnalysis);
    let normalized = ObligationCtxt::new(&infcx)
        .deeply_normalize(&ObligationCause::dummy(), ParamEnv::empty(), value)
        // alloc: the error's message
        .map_err(|errors| format!("`{value:?}` does not normalize: {errors:?}"))?;
    if normalized.has_aliases() || normalized.has_infer() {
        // alloc: the error's message
        return Err(format!("`{value:?}` normalizes to `{normalized:?}`, which has no content"));
    }
    Ok(normalized)
}

/// Writes each `TypeId` of a constant where const eval left the place of it: the first 16
/// bytes of its type's content ID, one value for a type in every crate of the build. `krate`
/// is the crate whose code holds the constant.
pub(crate) fn fill_type_ids(
    db: &Db,
    hashes: &HashSource,
    krate: Option<Crate>,
    blob: &mut ConstBlob,
) -> Result<(), String> {
    for (offset, ty) in std::mem::take(&mut blob.type_ids) {
        let id = hashes.ty(db, evaluated(db, krate, ty.as_ref())?)?;
        let at = offset as usize;
        let Some(bytes) = blob.bytes.get_mut(at..at + 16) else {
            // alloc: the error's message
            return Err(format!("a `TypeId` at {at}, past the end of its constant"));
        };
        bytes.copy_from_slice(&id.0[..16]);
    }
    Ok(())
}

/// The place of a function request a backend made of its own accord for a symbol the unit's
/// references lack: [`resolve_owning`] keeps it by name, [`resolve`] refuses it.
pub(crate) const OWN: u32 = u32::MAX;

/// Resolves a function's requests against the targets of its places, in a frame of the
/// worker's that `work` gets through [`Placed::frame`] and that is reset when it returns.
pub(crate) fn resolve<T>(places: &Places, refs: &Refs, work: impl FnOnce(&Placed<'_>) -> T) -> T {
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
        work(&Placed { places, refs, frame, symbols: OnceCell::new(), own: None })
    })
}

/// [`resolve`] for code a backend optimized, which may call symbols of its own the walk did
/// not list (LLVM's `bcmp`, `memcpy`, `__udivti3`): those are kept by name and returned, in
/// the order first called, for [`blaze_codegen::backend::Emitted::extra`].
pub(crate) fn resolve_owning<T>(
    places: &Places,
    refs: &Refs,
    work: impl FnOnce(&Placed<'_>) -> T,
) -> (T, Vec<String>) {
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
        let own = Some(RefCell::default());
        let placed = Placed { places, refs, frame, symbols: OnceCell::new(), own };
        let done = work(&placed);
        (done, placed.own.map(RefCell::into_inner).unwrap_or_default())
    })
}

/// A function's requests as the targets of its places. Its symbols are indexed on the first
/// relocation at one: most functions have none (a libcall, the personality), and each after
/// is one probe.
pub(crate) struct Placed<'a> {
    places: &'a Places,
    refs: &'a Refs,
    /// The unit's frame: its emit's tables are kept there.
    frame: &'a Frame,
    symbols: OnceCell<HashTable<(u64, u32), &'a Frame>>,
    /// The symbols the backend named that `refs` does not list, when it may name its own. Kept
    /// past the emit ([`blaze_codegen::backend::Emitted::extra`]).
    own: Option<RefCell<Vec<String>>>,
}

impl<'a> Placed<'a> {
    /// The frame the unit's emit runs in.
    pub(crate) fn frame(&self) -> &'a Frame {
        self.frame
    }
}

impl Placed<'_> {
    fn target(&self, place: u32) -> Result<Target, String> {
        let refs = self.refs;
        refs.get(place as usize)
            .cloned()
            // alloc: the error's message
            .ok_or_else(|| format!("place {place} of {} references", refs.len()))
    }
}

impl Resolver for Placed<'_> {
    fn func(&self, index: u32, request: &FuncRequest) -> Result<Target, String> {
        match self.places.funcs.get(index as usize) {
            Some(&OWN) => match &request.target {
                FuncTarget::Symbol(name) if self.own.is_some() => self.symbol(name),
                // alloc: the error's message
                _ => Err(format!("function {index} is the backend's own, and no symbol")),
            },
            Some(&place) => self.target(place),
            None => Err(format!("function {index} has no place")), // alloc: the error's message
        }
    }

    fn data(&self, index: u32, _: &DataRequest) -> Result<DataTarget, String> {
        match self.places.datas.get(index as usize) {
            Some(DataPlace::Place(place)) => self.target(*place).map(DataTarget::Target),
            Some(DataPlace::Position(span)) => Ok(DataTarget::Target(Target::Position(*span))),
            Some(DataPlace::Own) => Ok(DataTarget::Own),
            // clone: only a `file!()` text; `Resolver::data` returns an owned `DataTarget`
            Some(DataPlace::Text(text)) => Ok(DataTarget::Text(text.clone())),
            Some(DataPlace::Nowhere) => Ok(DataTarget::Nowhere),
            None => Err(format!("data {index} has no place")), // alloc: the error's message
        }
    }

    fn symbol(&self, name: &str) -> Result<Target, String> {
        let refs = self.refs;
        let frame = self.frame;
        let symbols = self.symbols.get_or_init(|| {
            let mut index: HashTable<(u64, u32), &Frame> = HashTable::new_in(frame);
            for (at, it) in refs.iter().enumerate() {
                if let Target::Extern(it) = it {
                    let hashed = symbol_hash(it);
                    index.insert_unique(hashed, (hashed, at as u32), |it| it.0);
                }
            }
            index
        });
        let same =
            |&(_, at): &(u64, u32)| matches!(&refs[at as usize], Target::Extern(it) if it == name);
        match symbols.find(symbol_hash(name), same) {
            // clone: `Resolver::symbol` returns an owned `Target`, as `target` does
            Some(&(_, at)) => Ok(refs[at as usize].clone()),
            None => match &self.own {
                Some(own) if !name.is_empty() => {
                    let mut own = own.borrow_mut();
                    if !own.iter().any(|it| it == name) {
                        // alloc: once per symbol of the backend's own, kept past the emit
                        own.push(name.to_owned());
                    }
                    // alloc: only a symbol of the backend's own; `Target::Extern` owns its name
                    Ok(Target::Extern(name.to_owned()))
                }
                // alloc: the error's message
                _ => Err(format!("the code calls `{name}`, which its references do not list")),
            },
        }
    }

    fn body(&self) -> Result<Target, String> {
        let refs = self.refs;
        match refs.len().checked_sub(1) {
            // clone: `Resolver::body` returns an owned `Target`, as `target` does
            Some(last) if last == self.places.refs as usize => Ok(refs[last].clone()),
            // alloc: the error's message
            _ => Err("a call of a fold's body from what is no thunk of it".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use cranelift_codegen::ir::LibCall;

    #[test]
    fn every_libcall_is_named_as_cranelift_names_it() {
        let name = cranelift_module::default_libcall_names();
        for &libcall in LibCall::all_libcalls() {
            assert_eq!(super::libcall_name(libcall), Ok(name(libcall).as_str()));
            assert_eq!(super::libcall_name(libcall).map(str::to_owned), Ok(super::libcall_symbol(libcall)));
        }
    }

    /// A unit's symbols are indexed in its frame, and found by name as before.
    #[test]
    fn a_symbol_is_found_through_the_frames_index() {
        use blaze_codegen::Target;

        use super::{super::convert::Resolver as _, resolve};
        let places = super::Places { funcs: Vec::new(), datas: Vec::new(), refs: 2 };
        let refs = [Target::Extern("memcpy".to_owned()), Target::Extern("memset".to_owned())];
        let found = resolve(&places, &refs, |it| [it.symbol("memset"), it.symbol("memcpy"), it.symbol("bcmp")]);
        let [memset, memcpy, bcmp] = found;
        assert_eq!(memset, Ok(refs[1].clone()));
        assert_eq!(memcpy, Ok(refs[0].clone()));
        assert!(bcmp.is_err());
    }
}
