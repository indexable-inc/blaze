//! Profile-guided optimization in the LLVM back end: rustc's `-Cprofile-generate` and
//! `-Cprofile-use` (fastcompile-sched 3; `docs/v2/plan/lanes/codegen/pgo.md`).
//!
//! A function is compiled alone in a module, where it is [`OWN`]. For the pre-link pipeline,
//! which is where LLVM instruments a function or reads its profile, it is given its [`name`]
//! instead: the name its counts are kept under, made of its item and type arguments, so the
//! same in the build that made the profile and in the build that uses it, and across edits of
//! its body. An edited function finds a record under its name with another hash, and is
//! counted ([`Prelinked::mismatched`](super::release::Prelinked)) rather than given the counts
//! of other code.
//!
//! Instrumented ([`Wanted::Generate`]), a function counts into its `__profc_` array, which the
//! object has in LLVM's counters section ([`COUNTERS`]): its code is read back with what it
//! refers to of the array as relocations in [`COUNTER_RELOCS`]
//! ([`super::emitted::read_profiled`]), and [`counted`] reads what LLVM's record of it says,
//! from which the link writes the program's records, names and counters. Two choices keep a
//! function's instrumented code its own:
//! - No value profiling (`-disable-vp`, [`super::set_options`]): an indirect call's target is
//!   named in the module by the request it is of, never by its own name, so a profile of
//!   targets would name nothing a build could promote, and its counters would be the link's
//!   to pool.
//! - No runtime hook: off Linux LLVM adds a function that refers to `__llvm_profile_runtime`
//!   to every module that has none of its own, which [`instrumented`] declares so it does not;
//!   the link pulls the runtime in for the program, as `-u__llvm_profile_runtime` does on
//!   Linux.
//!
//! A profile used ([`Wanted::Use`]) is the bytes [`give`] was given under its content ID:
//! LLVM reads it from memory, never from a file.

#![forbid(unsafe_code)]

use std::sync::{Arc, Weak};

use blaze_codegen::settings::Pgo;
use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
use inkwell::{
    context::Context,
    module::{Linkage, Module},
    types::AnyTypeEnum,
    values::{BasicValueEnum, FunctionValue},
};
use parking_lot::Mutex;

use super::{OWN, Unhandled};
use crate::backend::{Optimization, Passes};

/// The relocation namespace of the function's own PGO counters (index 0). Below
/// `blaze_stencil::tier::COUNTER_NAMESPACE`, above the shape's namespaces.
pub(crate) const COUNTER_RELOCS: u32 = u32::MAX - 4;

const _: () = assert!(
    COUNTER_RELOCS != super::sanitize::COVERAGE
        && COUNTER_RELOCS != blaze_stencil::tier::COUNTER_NAMESPACE
        && COUNTER_RELOCS != crate::v2::fold::BODY
        && COUNTER_RELOCS != crate::fragment::INLINED
);

/// The section of LLVM's PGO counters in ELF and Mach-O objects, and in COFF ones.
pub(crate) const COUNTERS: [&str; 2] = ["__llvm_prf_cnts", ".lprfc$M"];

/// What a function's code is built with for PGO, of [`Pgo`] other than `None`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Wanted {
    Generate,
    /// The profile [`give`] was given under this content ID.
    Use(ContentId),
}

impl Wanted {
    pub(crate) fn of(pgo: Pgo) -> Option<Wanted> {
        match pgo {
            Pgo::None => None,
            Pgo::Generate => Some(Wanted::Generate),
            Pgo::Use(id) => Some(Wanted::Use(id)),
        }
    }
}

/// Whether the pre-link pipeline of `optimization` runs PGO's passes itself: ThinLTO's
/// pre-link pipeline does, where LLVM puts them. Below `-O2` they run alone after the level's
/// own passes ([`super::Machinery::run_pgo`]): `default<O1>` would run them again at the
/// post-link, which runs with no PGO.
pub(crate) fn in_pipeline(optimization: Optimization) -> bool {
    matches!(optimization.passes, Passes::Default | Passes::Aggressive)
}

pub const NAME_DOMAIN: &str = "blaze v2 pgo name 1";

/// The name a function's counts are kept under: of its item, which a rename or a move
/// changes and an edit does not, and of its type arguments' facts. Two functions of one item
/// and arguments (a closure and its function) have one name, and two records under it, one
/// for each hash.
pub(crate) fn name(item: ItemKey, args: ContentId) -> String {
    blaze_codegen::profile_name(&name_bytes(item, args))
}

/// The bytes [`name`] spells: what [`blaze_codegen::ProfileRecord::name`] keeps.
pub(crate) fn name_bytes(item: ItemKey, args: ContentId) -> [u8; 16] {
    let id = Hasher::new(NAME_DOMAIN).id(&item.0).id(&args).finish();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&id.0[..16]);
    bytes
}

pub const PROFILE_DOMAIN: &str = "blaze v2 pgo profile 1";

/// The content ID of a profile, an indexed `.profdata` (what `llvm-profdata merge` writes):
/// what [`Pgo::Use`] names it by.
pub fn profile_id(bytes: &[u8]) -> ContentId {
    ContentId::of(PROFILE_DOMAIN, bytes)
}

/// A profile given to the process for the builds that use it, which they can read while it is
/// held: the caller holds it for the build.
#[derive(Debug)]
pub struct Profile {
    id: ContentId,
    _bytes: Arc<[u8]>,
}

impl Profile {
    pub fn id(&self) -> ContentId {
        self.id
    }
}

/// The profiles given, by content ID, while something holds them: a [`Profile`], or the
/// passes a thread keeps for one ([`super::Machinery`]).
// alloc: none until a profile is given; one entry per profile a build uses
static GIVEN: Mutex<Vec<(ContentId, Weak<[u8]>)>> = Mutex::new(Vec::new());

/// Gives the process the profile `bytes`, read from the store under `id`, for the builds whose
/// [`Pgo::Use`] names it while the [`Profile`] is held. Bytes whose content ID is not `id` are
/// refused.
pub fn give(id: ContentId, bytes: &[u8]) -> Result<Profile, String> {
    let found = profile_id(bytes);
    if found != id {
        // alloc: an error
        return Err(format!(
            "the profile read for {id:x} has the content ID {found:x}: the store gave other bytes"
        ));
    }
    let mut given = GIVEN.lock();
    given.retain(|it| it.1.strong_count() > 0);
    if let Some(kept) = given.iter().find(|it| it.0 == id).and_then(|it| it.1.upgrade()) {
        return Ok(Profile { id, _bytes: kept });
    }
    // alloc: once per profile given, its bytes and the NUL LLVM's reader wants after them
    let mut owned = Vec::with_capacity(bytes.len() + 1);
    owned.extend_from_slice(bytes);
    owned.push(0);
    let owned: Arc<[u8]> = owned.into();
    given.push((id, Arc::downgrade(&owned)));
    Ok(Profile { id, _bytes: owned })
}

/// The bytes of the profile given under `id`, with a NUL after them; an error naming it when
/// nothing holds it.
pub(super) fn profile(id: ContentId) -> Result<Arc<[u8]>, Unhandled> {
    let given = GIVEN.lock();
    given.iter().find(|it| it.0 == id).and_then(|it| it.1.upgrade()).ok_or_else(|| {
        // alloc: an error
        Unhandled::Llvm(format!(
            "the profile {id:x} is used and was not given: the build reads it from the store \
             and gives it (`hir_ty_codegen::v2::give_profile`) before it compiles"
        ))
    })
}

/// The name `function` has in the module while LLVM's PGO passes see it, given back
/// ([`Renamed::restore`]) before anything reads the module by [`OWN`].
pub(super) struct Renamed<'ctx> {
    function: FunctionValue<'ctx>,
}

impl<'ctx> Renamed<'ctx> {
    pub(super) fn restore(self) {
        self.function.as_global_value().set_name(OWN);
    }
}

/// Readies `function`, the module's [`OWN`], for LLVM's PGO passes: it takes its [`name`],
/// and, instrumented off Linux, the module declares the runtime's hook
/// (`__llvm_profile_runtime`) so LLVM adds no function that refers to it.
pub(super) fn instrumented<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    function: FunctionValue<'ctx>,
    wanted: Wanted,
    name: &str,
) -> Renamed<'ctx> {
    function.as_global_value().set_name(name);
    let triple = module.get_triple();
    let linux = triple.as_str().to_string_lossy().contains("-linux");
    if wanted == Wanted::Generate && !linux && module.get_global(RUNTIME_HOOK).is_none() {
        let hook = module.add_global(context.i32_type(), None, RUNTIME_HOOK);
        hook.set_linkage(Linkage::External);
    }
    Renamed { function }
}

/// The variable of the profile runtime that pulls it in (`INSTR_PROF_PROFILE_RUNTIME_VAR`).
const RUNTIME_HOOK: &str = "__llvm_profile_runtime";

/// What LLVM's record of an instrumented function (`__profd_`) says of it, for the link, which
/// writes the program's records, names and counters (LLVM's raw profile format).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Counted {
    /// The name its counts are kept under ([`name`]).
    pub(crate) name: String,
    /// The low 64 bits of the name's MD5, the record's `NameRef`.
    pub(crate) name_ref: u64,
    /// Its control flow's hash, the record's `FuncHash`.
    pub(crate) hash: u64,
    /// Its 8-byte counters, the record's `NumCounters`.
    pub(crate) counters: u32,
    /// The raw profile's version and kind, `__llvm_profile_raw_version`.
    pub(crate) version: u64,
}

/// What `module` (one function, instrumented) says of the function `name`: its record, its
/// counters and the raw profile's version. LLVM's record is a struct whose first two fields
/// are the name's MD5 and the hash.
pub(super) fn counted(module: &Module<'_>, name: &str) -> Result<Counted, Unhandled> {
    const SHAPE: Unhandled = Unhandled::Shape("an instrumented module whose PGO record is not read");
    let mut record = None;
    let mut counters = None;
    for global in module.get_globals() {
        let named = global.get_name().to_bytes();
        let (slot, value) = if named.starts_with(b"__profd_") {
            (&mut record, global)
        } else if named.starts_with(b"__profc_") {
            (&mut counters, global)
        } else {
            continue;
        };
        if slot.replace(value).is_some() {
            return Err(Unhandled::Shape("a module of two instrumented functions"));
        }
    }
    let (Some(record), Some(counters)) = (record, counters) else {
        return Err(Unhandled::Shape("an instrumented function without its PGO record"));
    };
    let Some(BasicValueEnum::StructValue(fields)) = record.get_initializer() else { return Err(SHAPE) };
    let field = |at| match fields.get_field_at_index(at) {
        Some(BasicValueEnum::IntValue(it)) => it.get_zero_extended_constant().ok_or(SHAPE),
        _ => Err(SHAPE),
    };
    let (name_ref, hash) = (field(0)?, field(1)?);
    let AnyTypeEnum::ArrayType(array) = counters.get_value_type() else { return Err(SHAPE) };
    let version = match module.get_global(RAW_VERSION).and_then(|it| it.get_initializer()) {
        Some(BasicValueEnum::IntValue(it)) => it.get_zero_extended_constant().ok_or(SHAPE)?,
        _ => return Err(Unhandled::Shape("an instrumented module without its raw profile version")),
    };
    // alloc: once per instrumented function, its name for the link
    Ok(Counted { name: name.to_owned(), name_ref, hash, counters: array.len(), version })
}

/// `INSTR_PROF_RAW_VERSION_VAR`.
const RAW_VERSION: &str = "__llvm_profile_raw_version";

/// The indexed profile, as `llvm-profdata merge` writes it, of `records`: each a function's
/// name, hash and counts. What a test uses a profile of.
#[cfg(test)]
pub(super) fn written(records: &[(&str, u64, &[u64])]) -> Vec<u8> {
    super::written_profile(records).unwrap()
}

#[cfg(test)]
mod tests {
    use inkwell::{memory_buffer::MemoryBuffer, targets::FileType};

    use super::*;
    use crate::{
        backend::Selection,
        llvm::{Machine, Machinery},
    };

    /// A branch on a compare: two blocks to count, and a branch for a profile to weigh.
    const BODY: &str = r#"
define i32 @"blaze.own"(ptr %p, i32 %n) {
entry:
  %small = icmp slt i32 %n, 10
  br i1 %small, label %load, label %store
load:
  %v = load i32, ptr %p
  ret i32 %v
store:
  store i32 %n, ptr %p
  ret i32 0
}
"#;

    const NONE: Optimization = Optimization { passes: Passes::None, selection: Selection::Fast };

    fn machinery(wanted: Option<Wanted>) -> Machinery {
        let machine = Machine::create(NONE).unwrap();
        match wanted {
            None => Machinery::new(machine),
            Some(wanted) => Machinery::profiled(machine, wanted).unwrap(),
        }
    }

    fn parsed<'ctx>(context: &'ctx Context, machinery: &Machinery) -> Module<'ctx> {
        let text = format!("{BODY}\0");
        let source = MemoryBuffer::create_from_memory_range_copy(text.as_bytes(), "test");
        let module = context.create_module_from_ir(source).unwrap();
        module.set_triple(&machinery.get_triple());
        module.set_data_layout(&machinery.get_target_data().get_data_layout());
        module
    }

    fn text(module: &Module<'_>) -> String {
        module.print_to_string().to_string_lossy().into_owned()
    }

    fn function_name() -> String {
        name(ItemKey(ContentId([7; 32])), ContentId([8; 32]))
    }

    /// The hash and counter count LLVM's instrumentation gives `BODY`, from its intrinsics.
    fn instrumented_hash() -> (u64, u32) {
        let context = Context::create();
        let machinery = machinery(None);
        let module = parsed(&context, &machinery);
        let name = function_name();
        let own = module.get_function(OWN).unwrap();
        let renamed = instrumented(&context, &module, own, Wanted::Generate, &name);
        machinery.run(&module, "pgo-instr-gen", false).unwrap();
        renamed.restore();
        let text = text(&module);
        let call = format!("@llvm.instrprof.increment(ptr @__profn_{name}, i64 ");
        let after = &text[text.find(&call).unwrap_or_else(|| panic!("{text}")) + call.len()..];
        let (hash, rest) = after.split_once(", i32 ").unwrap();
        let counters = rest.split_once(',').unwrap().0;
        (hash.parse().unwrap(), counters.parse().unwrap())
    }

    #[test]
    fn a_generated_module_has_the_instrumentation_intrinsics() {
        let (hash, counters) = instrumented_hash();
        assert_ne!(hash, 0);
        assert!(counters >= 1, "{counters}");
    }

    #[test]
    fn a_generated_module_counts_into_its_counters_and_says_so() {
        let context = Context::create();
        let machinery = machinery(Some(Wanted::Generate));
        let module = parsed(&context, &machinery);
        let name = function_name();
        let own = module.get_function(OWN).unwrap();
        let renamed = instrumented(&context, &module, own, Wanted::Generate, &name);
        machinery.run_pgo(&module).unwrap();
        renamed.restore();
        module.verify().unwrap();
        let text = text(&module);
        assert!(text.contains(&format!("@__profc_{name}")), "{text}");
        assert!(text.contains(COUNTERS[0]) || text.contains(COUNTERS[1]), "{text}");
        assert!(!text.contains("llvm.instrprof.increment"), "{text}");
        assert!(text.contains("define i32 @blaze.own("), "{text}");
        assert!(!text.contains("__llvm_profile_instrument_target"), "{text}");
        let counted = counted(&module, &name).unwrap();
        let (hash, counters) = instrumented_hash();
        assert_eq!((counted.hash, counted.counters), (hash, counters));
        assert_ne!(counted.name_ref, 0);
        assert_ne!(counted.version, 0);
        assert_eq!(counted.name, name);
        assert_eq!(machinery.take_mismatched(), 0);
    }

    #[test]
    fn an_instrumented_object_reads_back_with_its_counters() {
        let target = crate::Target::selected().unwrap();
        let context = Context::create();
        let machinery = machinery(Some(Wanted::Generate));
        let module = parsed(&context, &machinery);
        let own = module.get_function(OWN).unwrap();
        let renamed = instrumented(&context, &module, own, Wanted::Generate, &function_name());
        machinery.run_pgo(&module).unwrap();
        renamed.restore();
        let counted = counted(&module, &function_name()).unwrap();
        let object = machinery.write_to_memory_buffer(&module, FileType::Object).unwrap();
        let isa = crate::target().unwrap();
        let mut wants = crate::fragment::Wants::new(&**isa);
        let signature = cranelift_codegen::ir::Signature::new(isa.default_call_conv());
        let (code, _, bytes) = super::super::emitted::read_profiled(
            object.as_slice(),
            (OWN, signature.clone()),
            &mut wants,
            false,
        )
        .unwrap();
        assert_eq!(bytes, counted.counters * 8);
        let counts = code
            .relocs
            .iter()
            .filter(|it| it.name == crate::fragment::RelocName::User { namespace: COUNTER_RELOCS, index: 0 })
            .count();
        assert!(counts >= 1, "{:?}", code.relocs);
        // Read without PGO, the same object is refused.
        let mut wants = crate::fragment::Wants::new(&**isa);
        let plain = super::super::emitted::read(object.as_slice(), &[(OWN, signature)], &mut wants);
        assert!(plain.is_err() || plain.unwrap()[0].is_err(), "{:?}", target.triple());
    }

    /// `BODY` built with a profile of `records`: its text and how many functions the profile
    /// had a record of under their name with another hash.
    fn used(records: &[(&str, u64, &[u64])]) -> (String, u32) {
        let bytes = written(records);
        let id = profile_id(&bytes);
        let held = give(id, &bytes).unwrap();
        let context = Context::create();
        let machinery = machinery(Some(Wanted::Use(id)));
        let module = parsed(&context, &machinery);
        let own = module.get_function(OWN).unwrap();
        let renamed = instrumented(&context, &module, own, Wanted::Use(id), &function_name());
        machinery.run_pgo(&module).unwrap();
        renamed.restore();
        module.verify().unwrap();
        drop(held);
        (text(&module), machinery.take_mismatched())
    }

    #[test]
    fn a_profile_used_weighs_the_functions_branches() {
        let (hash, counters) = instrumented_hash();
        let name = function_name();
        let counts: Vec<u64> = (0..u64::from(counters)).map(|it| 1000 >> (it * 6)).collect();
        let (text, mismatched) = used(&[(&name, hash, &counts)]);
        assert_eq!(mismatched, 0);
        assert!(text.contains("!\"branch_weights\""), "{text}");
        assert!(text.contains("!\"function_entry_count\""), "{text}");
        let (plain, _) = used(&[("blaze.pgo.another", hash, &counts)]);
        assert!(!plain.contains("!\"branch_weights\""), "{plain}");
    }

    #[test]
    fn a_record_of_another_hash_is_counted_and_applies_nothing() {
        let (hash, counters) = instrumented_hash();
        let counts = vec![500; counters as usize];
        let (text, mismatched) = used(&[(&function_name(), hash ^ 1, &counts)]);
        assert_eq!(mismatched, 1);
        assert!(!text.contains("!\"branch_weights\""), "{text}");
        // A profile with no record of the name is no mismatch.
        let (_, missing) = used(&[("blaze.pgo.another", hash ^ 1, &counts)]);
        assert_eq!(missing, 0);
    }

    #[test]
    fn a_profile_is_given_by_its_content_and_read_while_held() {
        let bytes = written(&[("f", 1, &[1])]);
        let id = profile_id(&bytes);
        assert!(give(ContentId([9; 32]), &bytes).is_err());
        let held = give(id, &bytes).unwrap();
        assert_eq!(held.id(), id);
        let read = profile(id).unwrap();
        assert_eq!(&read[..bytes.len()], &bytes[..]);
        assert_eq!(read.last(), Some(&0));
        let again = give(id, &bytes).unwrap();
        assert!(Arc::ptr_eq(&again._bytes, &held._bytes));
        drop((held, again, read));
        assert!(profile(id).is_err());
    }

    #[test]
    fn the_name_is_the_items_and_arguments_and_nothing_else() {
        let a = name(ItemKey(ContentId([1; 32])), ContentId([2; 32]));
        assert_eq!(a, name(ItemKey(ContentId([1; 32])), ContentId([2; 32])));
        assert_ne!(a, name(ItemKey(ContentId([3; 32])), ContentId([2; 32])));
        assert_ne!(a, name(ItemKey(ContentId([1; 32])), ContentId([3; 32])));
        assert_eq!(a.len(), 42);
        assert!(a.bytes().all(|it| it.is_ascii_alphanumeric() || it == b'.'), "{a}");
    }

    #[test]
    fn pgo_runs_in_the_pre_link_pipeline_from_o2() {
        let at = |passes| in_pipeline(Optimization { passes, selection: Selection::Optimized });
        assert!(!at(Passes::None) && !at(Passes::Less));
        assert!(at(Passes::Default) && at(Passes::Aggressive));
        assert_eq!(Wanted::of(Pgo::None), None);
        assert_eq!(Wanted::of(Pgo::Generate), Some(Wanted::Generate));
        assert_eq!(Wanted::of(Pgo::Use(ContentId([4; 32]))), Some(Wanted::Use(ContentId([4; 32]))));
    }
}
