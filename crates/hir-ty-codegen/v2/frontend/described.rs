//! A function's describe as a store record (docs/v2/plan/design/store.md, "Describe
//! records"): its key, made of content before the describe runs, and its bytes, which name
//! every instance and static by content.
//!
//! A describe was a memo of one database (the `described_memo` query, gone since 2026-11-12: its key held interned ids and
//! the place of its units in a list of this process), so a new project in a warm daemon, a
//! restart and a second home described every function of the standard library their program
//! reaches again. The record is what another database of the same sources reads in its place.
//!
//! **The key** ([`HirFrontend::record_key_in`]) is the store's `Key::of` over the record's
//! schema, the code generator's code identity (the policy's toolchain id) and one digest of:
//!
//! - what describing reads of the build ([`Stage`]): the toolchain and target, how each
//!   origin's units build bodies ([`Units`], by content, never its place in [`UNITS`]),
//!   whether the build links an image and whether it is build-time code;
//! - what the units give whoever asks ([`Units::asked_as`]): the requester by what it is
//!   compiled as, never its package's name, so a second project of another name asks alike;
//! - the image key of the crate the instance is worked out in (its home,
//!   `hir_ty_mir::mir::home`): the content of that crate and, by its dependencies' keys, of
//!   every crate it loads, which is every crate the describe reads;
//! - the instance itself, ported ([`hir_ty::check_cache::port`]): its body by its item's key,
//!   its arguments as types by content.
//!
//! Only an instance whose home is read from an image has one, and only when every item and
//! crate it names is the home's or one of its dependencies' ([`Closure`], checked, never
//! assumed): the standard library's own, a dependency's, and their generics over each other's
//! types. Code an edit reaches (a workspace crate's, a generic over its types) has none and is
//! described by the walk's own task, as before (`frontend/witnesses.rs`).
//!
//! **The record** ([`HirFrontend::to_record_in`]) is the [`Described`] with its origin as
//! "the requester's" or the item's crate's own, each reference ported, and with each function
//! it refers to what the recursion limit reads of it ([`Counted`]): the walk asks that of a
//! callee before it describes it, and working it out reads the callee's optimized body
//! (`compiled_by_user`). Not kept: a describe that reads what the build calls a file
//! ([`names_files`]), which no key holds.
//!
//! What a database read from a record is remembered beside it ([`Found`]), for the three
//! asks that would otherwise run the describe after all: whether a kept describe still holds
//! ([`Frontend::redescribe`]), whether it is asked each build, and a callee's count.

use blaze_cas::Key;
use blaze_format::{
    Aligned, Part, SchemaHash,
    cursor::{Put as _, Reader},
    schema_of,
};
use blaze_hash::SharedMap;
use blaze_query::db::AsId as _;
use hir_ty::check_cache::port;

use super::*;

/// The domain of a describe record's inputs, and of its schema.
const DOMAIN: &str = "blaze v2 describe record 1";
const STAGE_DOMAIN: &str = "blaze v2 describe record stage 1";

/// The domain of [`HirFrontend::naming_of`].
const NAMING_DOMAIN: &str = "blaze v2 launch naming 1";

/// The domain of [`HirFrontend::static_key`].
const STATIC_DOMAIN: &str = "blaze v2 codegen static 1";

/// The first bytes of an ask ([`HirFrontend::ask_bytes`]): an ask of another layout is
/// refused by name, never read as this one.
const ASK_MAGIC: &[u8; 8] = b"bzask001";

/// The version of the record's own bytes, around its ported buffer.
const LAYOUT: u64 = 1;

/// The schema of a record: a record of another layout, or of another codec of types and ids,
/// has another key and is never found.
const SCHEMA: SchemaHash =
    schema_of(&[Part::Str(DOMAIN), Part::U64(LAYOUT), Part::Schema(port::SCHEMA)]);

/// An entry that names no id.
const NO_ENTRY: u32 = u32::MAX;

type R<T = ()> = Result<T, String>;

/// What a describe's record key holds of the build, beside the instance and who asks: made
/// once a front end ([`HirFrontend::with_policy`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Stage {
    /// The code generator's code identity (`Toolchain::id`): the record's producer.
    toolchain: ContentId,
    /// The toolchain whole (target, CPU, features) and the units, by content.
    units: ContentId,
}

impl Stage {
    pub(super) fn of(policy: &Policy, units: &Units) -> Stage {
        let mut hasher = Hasher::new(STAGE_DOMAIN);
        // alloc: once a build, the toolchain's fields as text
        hasher.part(format!("{:?}", policy.toolchain).as_bytes());
        units.hash_into(&mut hasher);
        Stage { toolchain: policy.toolchain.id, units: hasher.finish() }
    }
}

impl<T: std::fmt::Debug> ByOrigin<T> {
    /// Every value in one order: the kinds of a package the profile names no override for,
    /// the library's, then each package the profile names, by name.
    fn hash_into(&self, hasher: &mut Hasher) {
        let each = |hasher: &mut Hasher, value: &T| {
            // alloc: once a build, a few values as text
            hasher.part(format!("{value:?}").as_bytes());
        };
        each(hasher, &self.member);
        each(hasher, &self.dependency);
        each(hasher, &self.build_time);
        each(hasher, &self.std);
        // alloc: once a build, sized by the packages the profile names
        let mut named: Vec<(&str, &[T; 3])> =
            self.packages.iter().map(|(name, kinds)| (name.as_str(), kinds)).collect();
        named.sort_unstable_by_key(|it| it.0);
        hasher.update(&(named.len() as u64).to_le_bytes());
        for (name, kinds) in named {
            hasher.part(name.as_bytes());
            kinds.iter().for_each(|it| each(hasher, it));
        }
    }
}

impl Units {
    /// The units by content: what [`give_units`] compares, hashed.
    fn hash_into(&self, hasher: &mut Hasher) {
        let Units { inlines, any, tells, tells_any, names, names_any, options } = self;
        inlines.hash_into(hasher);
        tells.hash_into(hasher);
        names.hash_into(hasher);
        options.hash_into(hasher);
        hasher.update(&[u8::from(*any), u8::from(*tells_any), u8::from(*names_any)]);
    }

    /// What the units give code asked for by `origin`, into `hasher`: everything a describe
    /// reads of who asks ([`HirFrontend::compiled_for`] hands the origin on only to these).
    /// `None` when one of them is an error: such a describe fails, and is no record.
    fn asked_as(&self, origin: &Origin, hasher: &mut Hasher) -> Option<()> {
        let inlines = self.inlines.get(origin).as_ref().ok()?;
        let tells = self.tells.get(origin).as_ref().ok()?;
        let names = self.names.get(origin).as_ref().ok()?;
        let options = self.options.get(origin).as_ref().ok()?;
        hasher.update(&[u8::from(*inlines), u8::from(*tells), u8::from(*names)]);
        // alloc: once per key, two option sets as text
        hasher.part(format!("{options:?}").as_bytes());
        Some(())
    }
}

/// Under what a describe was read from its record: while both still are what they were, a
/// describe now would read the same record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Under {
    /// The image key of the instance's home.
    image: ContentId,
    /// [`HirFrontend::record_stage`]'s digest with what the units give the requester.
    asked: ContentId,
}

/// What one database read from describe records, kept for its life beside its kept walks.
#[derive(Default)]
pub(super) struct Found {
    /// Each describe read from a record, by the instance and who asked.
    records: SharedMap<(HirInstance, Origin), Under>,
    /// What the recursion limit reads of each function a record refers to
    /// ([`Counted`]), as the record holds it.
    counted: SharedMap<HirInstance, Counted>,
}

/// What each database read from records, each with the database's durable view it was read
/// in, the last asked for last: as many as kept walks' databases ([`KEPT_WALKS`]), a daemon's
/// few checkouts.
static FOUND: Lock<Vec<(base_db::Nonce, u64, Arc<Found>)>> =
    Lock::new("codegen.frontend.found", Vec::new());

/// What `db` read from records since the last write of an input a library's describe may
/// read (its durable view, as [`Frontend::durable_view`] gives it): after such a write
/// nothing read before it is kept, as a kept walk is not patched across one. A count
/// ([`Found::counted`]) is of a function by its id, and what an id names is the same only
/// while no library's source was written.
pub(super) fn found_of(db: &Db) -> Arc<Found> {
    let (database, _) = db.nonce_and_revision();
    let durable = blaze_query::db::Database::storage(db).durable_view().as_u64();
    let mut found = FOUND.lock();
    let entry = match found.iter().position(|(of, _, _)| *of == database) {
        Some(at) => {
            let (_, was, of) = found.remove(at);
            match was == durable {
                true => (database, durable, of),
                // alloc: once per write of a library's input, an empty table
                false => (database, durable, Arc::new(Found::default())),
            }
        }
        None => {
            if found.len() >= KEPT_WALKS {
                found.remove(0);
            }
            // alloc: once per database a process builds
            (database, durable, Arc::new(Found::default()))
        }
    };
    let of = entry.2.clone(); // clone: a reference count, once a build
    found.push(entry);
    of
}

/// The crates an instance worked out in `home` may name: `home` and what it depends on,
/// whose content `home`'s image key covers.
#[derive(Clone, Copy)]
struct Closure<'db> {
    db: &'db Db,
    crates: &'db [Crate],
}

impl<'db> Closure<'db> {
    fn of(db: &'db Db, home: Crate) -> Closure<'db> {
        Closure { db, crates: home.transitive_deps(db) }
    }

    fn holds(&self, krate: Crate) -> bool {
        self.crates.contains(&krate)
    }

    /// The name of `item` in a ported buffer: its item key, which is of its crate and path.
    /// Refused for an item of a crate outside the closure: the key would not cover it.
    fn name(&self, item: port::Item) -> R<port::Name> {
        use port::Item as I;
        let db = self.db;
        let id = match item {
            I::Function(it) => ItemId::Function(it),
            I::Struct(it) => ItemId::Adt(AdtId::StructId(it)),
            I::Union(it) => ItemId::Adt(AdtId::UnionId(it)),
            I::Enum(it) => ItemId::Adt(AdtId::EnumId(it)),
            I::Const(it) => ItemId::Const(it),
            I::Static(it) => ItemId::Static(it),
            I::Trait(it) => ItemId::Trait(it),
            I::TypeAlias(it) => ItemId::Alias(it),
            I::Impl(it) => ItemId::Impl(it),
            I::EnumVariant(it) => {
                self.inside(ItemId::Adt(AdtId::EnumId(it.lookup(db).parent)))?;
                return Ok(blaze_items::key::variant_key(db, it).0.0);
            }
        };
        self.inside(id)?;
        Ok(blaze_items::item_key(db, id).0.0)
    }

    fn inside(&self, item: ItemId) -> R {
        match self.holds(item.krate(self.db)) {
            true => Ok(()),
            // alloc: the reason no record is made
            false => Err("an item of a crate its home does not depend on".to_owned()),
        }
    }
}

/// The item of `db` that `name` names ([`Closure::name`]), if it has one.
fn item_named(db: &Db, name: &port::Name) -> Option<port::Item> {
    use blaze_items::key::Resolved;
    use port::Item as I;
    Some(match blaze_items::key::resolve(db, ItemKey(ContentId(*name)))? {
        Resolved::Item(ItemId::Function(it)) => I::Function(it),
        Resolved::Item(ItemId::Adt(AdtId::StructId(it))) => I::Struct(it),
        Resolved::Item(ItemId::Adt(AdtId::UnionId(it))) => I::Union(it),
        Resolved::Item(ItemId::Adt(AdtId::EnumId(it))) => I::Enum(it),
        Resolved::Item(ItemId::Const(it)) => I::Const(it),
        Resolved::Item(ItemId::Static(it)) => I::Static(it),
        Resolved::Item(ItemId::Trait(it)) => I::Trait(it),
        Resolved::Item(ItemId::Alias(it)) => I::TypeAlias(it),
        Resolved::Item(ItemId::Impl(it)) => I::Impl(it),
        Resolved::Variant(it) => I::EnumVariant(it),
        _ => return None,
    })
}

/// The tags of an instance in a record.
const CODE: u8 = 0;
const ASM: u8 = 1;
const START: u8 = 2;

/// The tags of a reference in a record, after an instance's.
const STATIC: u8 = 3;
const EXTERN: u8 = 4;
const WEAK: u8 = 5;

/// `instance` into `out`, its ids and arguments into `writer`. `None` for one with no
/// portable form: an environment that is not empty, a home outside `closure`, a function of
/// the allocator shim.
fn put_instance<'db>(
    closure: &Closure<'db>,
    writer: &mut port::Writer<'db>,
    out: &mut Vec<u8>,
    instance: &HirInstance,
) -> Option<()> {
    let db = closure.db;
    match &instance.0 {
        Kind::Code(code, env) => {
            if env.param_env(db) != ParamEnv::empty() || !closure.holds(env.krate) {
                return None;
            }
            let (def, entry) = match code.def {
                InstanceDef::Fn(func) => (0u8, writer.function(func)),
                InstanceDef::Closure(id) => (1, writer.closure(InternedClosureId::from_id(id))),
                InstanceDef::Coroutine(id) => {
                    (2, writer.coroutine(InternedCoroutineId::from_id(id)))
                }
                InstanceDef::CoroutineByMove(id) => {
                    (3, writer.coroutine(InternedCoroutineId::from_id(id)))
                }
                InstanceDef::DropGlue => (4, NO_ENTRY),
                InstanceDef::CallOnceShim => (5, NO_ENTRY),
                InstanceDef::VtableShim(func) => (6, writer.function(func)),
                InstanceDef::FnPointerShim => (7, NO_ENTRY),
                InstanceDef::ReifyShim { receiver: false } => (8, NO_ENTRY),
                InstanceDef::ReifyShim { receiver: true } => (9, NO_ENTRY),
            };
            out.put_u8(CODE);
            out.put_u8(def);
            out.put_u32(entry);
            out.put_u32(writer.args(code.args.as_ref()));
            out.put_bytes(&blaze_items::key::crate_key(db, env.krate).0);
        }
        Kind::Asm(text, image) => {
            out.put_u8(ASM);
            out.put_bytes_u32(text.as_bytes());
            out.put_u8(u8::from(*image));
        }
        Kind::Start(main) => {
            if !closure.holds(main.krate(db)) {
                return None;
            }
            out.put_u8(START);
            out.put_u32(writer.function(*main));
        }
        Kind::Allocator(_) => return None,
    }
    Some(())
}

/// The instance [`put_instance`] wrote with tag `tag`, made in `db`.
fn get_instance<'db>(
    db: &'db Db,
    ported: &mut port::Reader<'_, 'db>,
    bytes: &mut Reader<'_>,
    tag: u8,
) -> R<HirInstance> {
    Ok(HirInstance(match tag {
        CODE => {
            let def = bytes.u8()?;
            let entry = bytes.u32()?;
            let args = ported.args(bytes.u32()?)?;
            let krate = ContentId(*bytes.array::<32>()?);
            let def = match def {
                0 => InstanceDef::Fn(ported.function(entry)?),
                1 => InstanceDef::Closure(ported.closure(entry)?.as_id()),
                2 => InstanceDef::Coroutine(ported.coroutine(entry)?.as_id()),
                3 => InstanceDef::CoroutineByMove(ported.coroutine(entry)?.as_id()),
                4 => InstanceDef::DropGlue,
                5 => InstanceDef::CallOnceShim,
                6 => InstanceDef::VtableShim(ported.function(entry)?),
                7 => InstanceDef::FnPointerShim,
                8 => InstanceDef::ReifyShim { receiver: false },
                9 => InstanceDef::ReifyShim { receiver: true },
                // alloc: the reason of a refusal
                other => return Err(format!("an instance of no kind ({other})")),
            };
            let krate = blaze_items::key::resolve_crate(db, &krate)
                .ok_or("an instance whose home is no crate of this database")?;
            let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
            Kind::Code(Instance { def, args: args.store(), checked: false }, env)
        }
        ASM => {
            let text = bytes.str_u32()?.to_owned(); // alloc: the instance's own text
            Kind::Asm(text, flag(bytes.u8()?)?)
        }
        START => Kind::Start(ported.function(bytes.u32()?)?),
        // alloc: the reason of a refusal
        other => return Err(format!("a reference of no kind ({other})")),
    }))
}

fn flag(byte: u8) -> R<bool> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        // alloc: the reason of a refusal
        other => Err(format!("a flag that is neither 0 nor 1 ({other})")),
    }
}

fn put_id(out: &mut Vec<u8>, id: &ContentId) {
    out.put_bytes(&id.0);
}

fn get_id(bytes: &mut Reader<'_>) -> R<ContentId> {
    Ok(ContentId(*bytes.array::<32>()?))
}

fn put_text(out: &mut Vec<u8>, text: Option<&str>) {
    match text {
        Some(text) => {
            out.put_u8(1);
            out.put_bytes_u32(text.as_bytes());
        }
        None => out.put_u8(0),
    }
}

fn get_text(bytes: &mut Reader<'_>) -> R<Option<String>> {
    Ok(match flag(bytes.u8()?)? {
        true => Some(bytes.str_u32()?.to_owned()), // alloc: the describe's own text
        false => None,
    })
}

/// An origin by its kind and its package's name; tag 0 is "the requester's", which only a
/// describe's own origin is written as.
fn put_origin(out: &mut Vec<u8>, origin: &Origin) {
    let (tag, name) = match origin {
        Origin::Std => (1u8, ""),
        Origin::Dependency(name) => (2, name.as_str()),
        Origin::BuildTime(name) => (3, name.as_str()),
        Origin::Member(name) => (4, name.as_str()),
    };
    out.put_u8(tag);
    out.put_bytes_u32(name.as_bytes());
}

fn get_origin(bytes: &mut Reader<'_>, requester: &Origin) -> R<Origin> {
    let tag = bytes.u8()?;
    if tag == 0 {
        return Ok(*requester);
    }
    origin_tagged(bytes, tag)
}

/// The origin [`put_origin`] wrote with `tag`, which is not 0.
fn origin_tagged(bytes: &mut Reader<'_>, tag: u8) -> R<Origin> {
    let name = Package::new(bytes.str_u32()?);
    Ok(match tag {
        1 => Origin::Std,
        2 => Origin::Dependency(name),
        3 => Origin::BuildTime(name),
        4 => Origin::Member(name),
        // alloc: the reason of a refusal
        other => return Err(format!("an origin of no kind ({other})")),
    })
}

fn put_counted(out: &mut Vec<u8>, counted: &Counted) {
    let Counted { body, limit, heads_own_path } = counted;
    let flags = u8::from(body.is_some())
        | u8::from(body.is_some_and(|it| it.drop_glue)) << 1
        | u8::from(limit.is_some()) << 2
        | u8::from(*heads_own_path) << 3;
    out.put_u8(flags);
    put_id(out, &body.map_or(ContentId::default(), |it| it.body));
    put_id(out, &limit.map_or(ContentId::default(), |it| it.krate));
    out.put_u32(limit.map_or(0, |it| it.value));
}

fn get_counted(bytes: &mut Reader<'_>) -> R<Counted> {
    let flags = bytes.u8()?;
    if flags >= 1 << 4 {
        // alloc: the reason of a refusal
        return Err(format!("a count with flags {flags:#x}"));
    }
    let (body, krate, value) = (get_id(bytes)?, get_id(bytes)?, bytes.u32()?);
    Ok(Counted {
        body: (flags & 1 != 0).then_some(Instantiates { body, drop_glue: flags & 2 != 0 }),
        limit: (flags & 4 != 0).then_some(Limit { krate, value }),
        heads_own_path: flags & 8 != 0,
    })
}

/// The image key of the crate of `value`; `None` for a crate read from no image (a
/// workspace crate's, [`HirFrontend::static_key`]).
fn static_image(db: &Db, value: &HirStatic) -> Option<ContentId> {
    Some(hir_def::deps::image_of(db, value.0.krate(db)).as_ref()?.key())
}

/// [`HirFrontend::static_key`] over its inputs, the one place it is made.
fn static_key_over(
    producer: ContentId,
    stage: &ContentId,
    image: &ContentId,
    id: &blaze_codegen::StaticId,
) -> Key {
    let mut inputs = Hasher::new(STATIC_DOMAIN);
    inputs.id(stage).id(image).id(&id.0.0).id(&id.1);
    Key::of(SCHEMA, producer, &[inputs.finish()])
}

/// [`HirFrontend::ask_bytes`], given the database.
fn ask_written(db: &Db, (instance, origin): &(HirInstance, Origin)) -> Option<Vec<u8>> {
    let closure = match &instance.0 {
        Kind::Code(_, env) => Closure::of(db, env.krate),
        Kind::Start(main) => Closure::of(db, main.krate(db)),
        // An `asm!` function names no item; the allocator shim's has no portable form,
        // which `put_instance` says.
        Kind::Asm(..) | Kind::Allocator(_) => Closure { db, crates: &[] },
    };
    let mut writer = port::Writer::new(db, DbInterner::new_no_crate(db));
    // alloc: once per ask written, the instance's few words
    let mut named = Vec::with_capacity(64);
    put_instance(&closure, &mut writer, &mut named, instance)?;
    let ported = writer.finish(&mut |item| closure.name(item)).ok()?;
    // alloc: once per ask written, its bytes, which the launched program keeps
    let mut out = Vec::with_capacity(ASK_MAGIC.len() + 4 + ported.len() + 64 + named.len());
    out.put_bytes(ASK_MAGIC);
    out.put_bytes_u32(&ported);
    put_origin(&mut out, origin);
    out.extend_from_slice(&named);
    Some(out)
}

/// [`HirFrontend::ask_of`], given the database.
fn ask_read(db: &Db, bytes: &[u8]) -> R<(HirInstance, Origin)> {
    let mut bytes = Reader::new(bytes);
    let magic = bytes.array::<8>()?;
    if magic != ASK_MAGIC {
        // alloc: the reason of a refusal
        return Err(format!("its first bytes are {magic:?}, not `bzask001`: an ask of another layout"));
    }
    // The ported buffer is read in place at its own alignment, which bytes at an offset
    // do not have.
    let ported = Aligned::copy_from(bytes.bytes_u32()?);
    let mut ported = port::Reader::open(
        db,
        DbInterner::new_no_crate(db),
        &ported,
        &mut |name| item_named(db, name),
    )?;
    let origin = match bytes.u8()? {
        0 => {
            return Err("its origin is \"the requester's\" (tag 0), which only a describe's \
                        own origin is written as"
                .to_owned()); // alloc: the reason of a refusal
        }
        tag => origin_tagged(&mut bytes, tag)?,
    };
    let tag = bytes.u8()?;
    let instance = get_instance(db, &mut ported, &mut bytes, tag)?;
    bytes.finish()?;
    Ok((instance, origin))
}

impl<'db> HirFrontend<'db> {
    /// What a record key holds of this front end: its producer, and its [`Stage`] with
    /// whether the build links an image and is build-time code. `None` for a front end whose
    /// describes are no records: one made with no policy, one keyed by the tests' hashes
    /// (ids of a database), and one whose bodies are walked against a program's dyn facts
    /// (its describes are of that program, [`Facing::world`]).
    fn record_stage(&self) -> Option<(ContentId, ContentId)> {
        let stage = self.stage?;
        let by_content = match &self.hashes {
            HashSource::Items(_) => true,
            #[cfg(test)]
            HashSource::ById(_) => false,
        };
        if !by_content || self.devirt.world().is_some() {
            return None;
        }
        let mut hasher = Hasher::new(STAGE_DOMAIN);
        hasher.id(&stage.units).update(&[u8::from(self.image), u8::from(self.build_time.0)]);
        Some((stage.toolchain, hasher.finish()))
    }

    /// The image key of the home of `instance`, and the crates it may name; `None` for an
    /// instance with no home read from an image, or worked out in an environment that is not
    /// empty.
    fn sealed(db: &'db Db, instance: &HirInstance) -> Option<(ContentId, Closure<'db>)> {
        let Kind::Code(_, env) = &instance.0 else { return None };
        if env.param_env(db) != ParamEnv::empty() {
            return None;
        }
        let image = hir_def::deps::image_of(db, env.krate).as_ref()?.key();
        Some((image, Closure::of(db, env.krate)))
    }

    /// What a describe of `instance` for `requester` is read under ([`Under`]) and its
    /// producer; `None` where it has no record key.
    fn under(
        &self,
        db: &'db Db,
        instance: &HirInstance,
        requester: &Origin,
    ) -> Option<(ContentId, Under, Closure<'db>)> {
        let (producer, stage) = self.record_stage()?;
        let (image, closure) = Self::sealed(db, instance)?;
        let mut asked = Hasher::new(STAGE_DOMAIN);
        asked.id(&stage);
        self.units.as_ref()?.asked_as(requester, &mut asked)?;
        Some((producer, Under { image, asked: asked.finish() }, closure))
    }

    /// [`Frontend::record_key`], given the database.
    pub(super) fn record_key_in(
        &self,
        db: &'db Db,
        instance: &HirInstance,
        requester: &Origin,
    ) -> Option<Key> {
        let (producer, under, closure) = self.under(db, instance, requester)?;
        let mut writer = port::Writer::new(db, DbInterner::new_no_crate(db));
        // alloc: once per key, the instance's few words
        let mut named = Vec::with_capacity(64);
        put_instance(&closure, &mut writer, &mut named, instance)?;
        let ported = writer.finish(&mut |item| closure.name(item)).ok()?;
        let mut inputs = Hasher::new(DOMAIN);
        inputs.id(&under.asked).id(&under.image).part(&named).part(&ported);
        Some(Key::of(SCHEMA, producer, &[inputs.finish()]))
    }

    /// What the build calls the files of `instance`'s home crate and of each crate it loads:
    /// `Hasher("blaze v2 launch naming 1")` over, for the home then each crate of
    /// `Crate::transitive_deps` in its order, the name of the crate's root file as this front
    /// end's hashes give it (`HashSource::Items`' names, blaze-cargo's `frontend_over`), each
    /// length first, their count before them. `None` where [`Self::sealed`] is `None` or a
    /// root has no name.
    ///
    /// A describe record's key holds content, never place, so one record serves every machine
    /// ([`Self::record_key_in`]); but the code a launch binds holds names of files (`file!()`,
    /// a panic's location, the span tables its positions are read from), and those differ by
    /// machine: a registry crate's root is under `$CARGO_HOME`. A bind key covers this beside
    /// the describe key (rule 21), so a record bound on one machine is never served on another
    /// that names the files otherwise. Every other file of a crate is named under its root's
    /// directory by its path there, which the crate's content (its image key) covers.
    pub fn naming_of(&self, instance: &HirInstance) -> Option<ContentId> {
        self.with(|db| self.naming_in(db, instance))
    }

    /// [`Self::naming_of`], given the database.
    fn naming_in(&self, db: &'db Db, instance: &HirInstance) -> Option<ContentId> {
        let (_, closure) = Self::sealed(db, instance)?;
        self.naming_over(db, closure.crates)
    }

    /// [`Self::naming_of`] over `crates`, in their order.
    fn naming_over(&self, db: &Db, crates: &[Crate]) -> Option<ContentId> {
        let mut hasher = Hasher::new(NAMING_DOMAIN);
        hasher.update(&(crates.len() as u64).to_le_bytes());
        for krate in crates {
            // alloc: each root's name, once a bind key is made
            let name = self.hashes.file_name(db, krate.data(db).root_file_id).ok()?;
            hasher.part(name.as_bytes());
        }
        Some(hasher.finish())
    }

    /// `asked` (an instance and whose code asks for it) as bytes another database of the same
    /// sources reads back ([`Self::ask_of`]) with no handle of this one: what a launched
    /// program holds for each slot, so a first call after a daemon start is asked for with no
    /// front end's handle surviving the start. Little-endian:
    ///
    /// `"bzask001"` | u32 length + the ported buffer ([`port::Writer`]) | the origin
    /// ([`put_origin`], tags 1 to 4) | the instance ([`put_instance`], its tag first).
    ///
    /// Its items are named by their item keys and its types by content, as a describe record's
    /// are, over the crates its home may name ([`Closure`]): the home is `env.krate` of a
    /// function, `main`'s crate of the C `main`, and no crate of an `asm!` function, which
    /// names no item. `None` for an instance with no portable form: the allocator shim's, one
    /// worked out in an environment that is not empty, and one naming an item of a crate its
    /// home does not load. Unlike a record key it needs no image: a workspace function's ask is
    /// bytes too, and its items' keys name it in the next database as in this one.
    pub fn ask_bytes(&self, asked: &(HirInstance, Origin)) -> Option<Vec<u8>> {
        self.with(|db| ask_written(db, asked))
    }

    /// The ask [`Self::ask_bytes`] wrote, made in this database. An `Err` names why it does
    /// not read: another magic (an ask of another layout), an origin of tag 0 (only a
    /// describe's own origin is "the requester's"), an item this database has none of, bytes
    /// cut short or left over. Never a panic: the bytes are a store's.
    pub fn ask_of(&self, bytes: &[u8]) -> Result<(HirInstance, Origin), String> {
        self.with(|db| ask_read(db, bytes)).map_err(|why| {
            // alloc: the reason of a refusal
            format!("an ask of {} bytes does not read in this database: {why}", bytes.len())
        })
    }

    /// The key of the static `value` (its id in a build `id`) of a crate with an image: what a
    /// launched program's bind record of the static is keyed by, beside its naming
    /// ([`Self::static_naming_of`]) and the launch's policy, symbols and record maker (the
    /// bind key's own inputs). `Key::of` over the describe records' schema, this front end's
    /// producer and `Hasher("blaze v2 codegen static 1")` of: the stage
    /// ([`Self::record_stage`]: the toolchain whole, the units of every origin by content,
    /// whether the build links an image and is build-time code), the image key of the
    /// static's crate, and `id` (its item key, which holds its crate's key with its origin,
    /// and which instance of it).
    ///
    /// Why that is every input of its bytes (rule 21): a static's value is its initializer
    /// evaluated in its own crate (no static is generic, so none is compiled for whoever asks),
    /// and it reads that crate and the crates it loads, which the image key covers by content
    /// (as a describe's home's does, [`Closure`]); how it is compiled is the stage's. What its
    /// positions call files is the naming's.
    ///
    /// `None` where [`Self::record_stage`] is (no policy, the tests' hashes, a program's dyn
    /// facts), and for a static of a crate with no image: a workspace crate's, whose content
    /// no key holds before the build lowers it, so its record is made by each build; it is in
    /// the start image as a bound static and never a bind record, nor is a function naming it.
    pub fn static_key(&self, value: &HirStatic, id: &blaze_codegen::StaticId) -> Option<Key> {
        let (producer, stage) = self.record_stage()?;
        let image = self.with(|db| static_image(db, value))?;
        Some(static_key_over(producer, &stage, &image, id))
    }

    /// What the build calls the root files of the static's crate and of every crate it loads
    /// ([`Self::naming_of`] for a static): its value's positions name them (a `Location`'s
    /// file, a `file!()`), so a bind record of it bound on one machine is never served on
    /// another that names them otherwise. `None` where a root has no name.
    pub fn static_naming_of(&self, value: &HirStatic) -> Option<ContentId> {
        self.with(|db| self.naming_over(db, value.0.krate(db).transitive_deps(db)))
    }

    /// [`Frontend::to_record`], given the database.
    pub(super) fn to_record_in(
        &self,
        db: &'db Db,
        instance: &HirInstance,
        requester: &Origin,
        described: &Described<HirInstance, HirStatic>,
    ) -> Option<Vec<u8>> {
        let (_, _, closure) = self.under(db, instance, requester)?;
        // Described by the build's own front end each build: what the build calls the file
        // is in no key.
        if names_files(db, &instance.0) {
            return None;
        }
        // Whose code it is, by the one rule ([`Self::compiled_for`]): its crate's own, or
        // whoever asks.
        let own = match crate_of(db, &instance.0) {
            Some(krate) if !user_compiled(db, &instance.0) => Some(self.origin(db, krate)),
            _ => None,
        };
        if described.origin != own.unwrap_or(*requester) {
            return None;
        }
        let mut writer = port::Writer::new(db, DbInterner::new_no_crate(db));
        // alloc: per record written, its bytes
        let mut out = Vec::with_capacity(512);
        let Described {
            item,
            code,
            template_code,
            anchors,
            built_in_code,
            args,
            origin: _,
            editable,
            over_editable,
            overflow_checks,
            address,
            symbol,
            linkage,
            refs,
            built_in,
            name,
            owner,
            identity,
            abi,
            bound,
            cold,
        } = described;
        put_id(&mut out, &item.0);
        put_id(&mut out, &code.0);
        put_id(&mut out, &template_code.0);
        put_id(&mut out, &built_in_code.0);
        put_id(&mut out, args);
        put_id(&mut out, identity);
        put_id(&mut out, abi);
        match anchors {
            Some(anchors) => {
                out.put_u8(1);
                anchors.iter().for_each(|it| put_id(&mut out, it));
            }
            None => out.put_u8(0),
        }
        match &own {
            Some(own) => put_origin(&mut out, own),
            None => out.put_u8(0),
        }
        let flags = u8::from(*editable)
            | u8::from(*over_editable) << 1
            | u8::from(*cold) << 2
            | u8::from(matches!(address, Address::Significant)) << 3
            | u8::from(matches!(linkage, blaze_codegen::Linkage::Weak)) << 4
            | u8::from(matches!(owner, Owner::Workspace)) << 5;
        out.put_u8(flags);
        out.put_u8(match overflow_checks {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        });
        put_text(&mut out, symbol.as_deref());
        put_text(&mut out, Some(name.as_str()));
        match bound {
            Ok(bound) => {
                out.put_u8(1);
                put_id(&mut out, bound);
            }
            Err(why) => {
                out.put_u8(0);
                out.put_bytes_u32(why.as_bytes());
            }
        }
        out.put_u32(u32::try_from(built_in.len()).ok()?);
        built_in.iter().for_each(|it| out.put_u32(*it));
        out.put_u32(u32::try_from(refs.len()).ok()?);
        for it in refs {
            match it {
                FrontRef::Function(callee) => {
                    put_instance(&closure, &mut writer, &mut out, callee)?;
                    put_counted(&mut out, &counted(db, &callee.0));
                }
                FrontRef::Static(value) => {
                    if !closure.holds(value.0.krate(db)) {
                        return None;
                    }
                    out.put_u8(STATIC);
                    out.put_u32(writer.static_item(value.0));
                }
                FrontRef::Extern(name) => {
                    out.put_u8(EXTERN);
                    out.put_bytes_u32(name.as_bytes());
                }
                FrontRef::Weak(name) => {
                    out.put_u8(WEAK);
                    out.put_bytes_u32(name.as_bytes());
                }
            }
        }
        let ported = writer.finish(&mut |item| closure.name(item)).ok()?;
        // alloc: per record written, its bytes: the ported buffer, then the describe
        let mut record = Vec::with_capacity(4 + ported.len() + out.len());
        record.put_bytes_u32(&ported);
        record.extend_from_slice(&out);
        Some(record)
    }

    /// [`Frontend::from_record`], given the database: the describe, and into [`Found`] that
    /// it was read and what its record says of the functions it refers to.
    pub(super) fn from_record_in(
        &self,
        db: &'db Db,
        instance: &HirInstance,
        requester: &Origin,
        record: &[u8],
    ) -> R<Described<HirInstance, HirStatic>> {
        let (_, under, _) = (self.under(db, instance, requester))
            .ok_or("this front end has no record key for it now")?;
        let found = self.found.as_ref().ok_or("this front end was made with no policy")?;
        let mut bytes = Reader::new(record);
        // The ported buffer is read in place at its own alignment, which a store's bytes at
        // an offset do not have.
        let ported = Aligned::copy_from(bytes.bytes_u32()?);
        let mut ported = port::Reader::open(
            db,
            DbInterner::new_no_crate(db),
            &ported,
            &mut |name| item_named(db, name),
        )?;
        let item = ItemKey(get_id(&mut bytes)?);
        let code = CodeHash(get_id(&mut bytes)?);
        let template_code = CodeHash(get_id(&mut bytes)?);
        let built_in_code = CodeHash(get_id(&mut bytes)?);
        let args = get_id(&mut bytes)?;
        let identity = get_id(&mut bytes)?;
        let abi = get_id(&mut bytes)?;
        let anchors = match flag(bytes.u8()?)? {
            true => {
                let mut anchors = [ContentId::default(); blaze_codegen::template::OWN_TABLES];
                for anchor in &mut anchors {
                    *anchor = get_id(&mut bytes)?;
                }
                Some(anchors)
            }
            false => None,
        };
        let origin = get_origin(&mut bytes, requester)?;
        let flags = bytes.u8()?;
        if flags >= 1 << 6 {
            // alloc: the reason of a refusal
            return Err(format!("a describe with flags {flags:#x}"));
        }
        let overflow_checks = match bytes.u8()? {
            0 => None,
            1 => Some(false),
            2 => Some(true),
            // alloc: the reason of a refusal
            other => return Err(format!("overflow checks of no kind ({other})")),
        };
        let symbol = get_text(&mut bytes)?;
        let name = get_text(&mut bytes)?.ok_or("a describe with no name")?;
        let bound = match flag(bytes.u8()?)? {
            true => Ok(get_id(&mut bytes)?),
            false => Err(bytes.str_u32()?.to_owned()), // alloc: the describe's own text
        };
        let count = bytes.count_u32(size_of::<u32>())?;
        // alloc: the describe's own list
        let mut built_in = Vec::with_capacity(count);
        for _ in 0..count {
            built_in.push(bytes.u32()?);
        }
        let count = bytes.count_u32(1)?;
        // alloc: the describe's own list
        let mut refs = Vec::with_capacity(count);
        for _ in 0..count {
            refs.push(match bytes.u8()? {
                STATIC => FrontRef::Static(HirStatic(ported.static_item(bytes.u32()?)?)),
                EXTERN => FrontRef::Extern(bytes.str_u32()?.to_owned()), // alloc: the describe's own text
                WEAK => FrontRef::Weak(bytes.str_u32()?.to_owned()), // alloc: the describe's own text
                tag => {
                    let callee = get_instance(db, &mut ported, &mut bytes, tag)?;
                    let counted = get_counted(&mut bytes)?;
                    // clone: the map's key, once per reference of a record read
                    found.counted.pin().insert(callee.clone(), counted);
                    FrontRef::Function(callee)
                }
            });
        }
        bytes.finish()?;
        // clone: the map's key, once per record read
        found.records.pin().insert((instance.clone(), *requester), under);
        Ok(Described {
            item,
            code,
            template_code,
            anchors,
            built_in_code,
            args,
            origin,
            editable: flags & 1 != 0,
            over_editable: flags & 2 != 0,
            overflow_checks,
            address: if flags & 8 != 0 { Address::Significant } else { Address::Free },
            symbol,
            linkage: match flags & 16 != 0 {
                true => blaze_codegen::Linkage::Weak,
                false => blaze_codegen::Linkage::Strong,
            },
            refs,
            built_in,
            name,
            owner: if flags & 32 != 0 { Owner::Workspace } else { Owner::Dependency },
            identity,
            abi,
            bound,
            cold: flags & 4 != 0,
        })
    }

    /// Whether the describe of `instance` for `requester` was read from a record in this
    /// database and a describe now would read that record again: the home's image and what
    /// the build gives whoever asks are what they were ([`Under`]). The record's key is of
    /// those and of the instance, whose ids name the same content while the image is the
    /// same, so the describe still holds with no describe and no key made again.
    pub(super) fn found_holds(
        &self,
        db: &'db Db,
        instance: &HirInstance,
        requester: &Origin,
    ) -> bool {
        let Some(found) = &self.found else { return false };
        // clone: the map's key by value, a few `Copy` ids in a `Clone`-only type
        let Some(was) = found.records.pin().get(&(instance.clone(), *requester)).copied() else {
            return false;
        };
        self.under(db, instance, requester).is_some_and(|(_, now, _)| now == was)
    }

    /// What a record this database read says the recursion limit reads of `instance`.
    pub(super) fn found_counted(&self, instance: &HirInstance) -> Option<Counted> {
        self.found.as_ref()?.counted.pin().get(instance).copied()
    }
}

#[cfg(test)]
mod tests {
    use base_db::{Crate, FileId, all_crates};
    use blaze_codegen::{
        backend::FrontRef,
        plan::{Origin, Package},
    };
    use blaze_ids::ContentId;
    use blaze_items::ItemKey;
    use hegel::{Hegel, Settings, TestCase, generators as gs};
    use hir_expand::fixture;
    use hir_ty::{
        check_cache::port,
        next_solver::{DbInterner, GenericArgs, ParamEnv},
    };

    use super::{
        BuildTime, Closure, Databases, HashSource, HirFrontend, put_instance, static_key_over,
    };
    use crate::{
        Instance,
        v2::{HirInstance, HirStatic, ItemHashes, Kind, refs},
    };

    /// `main` loads `a` (which loads `c`) and `b`; `x` is loaded by nothing; `c` has a second
    /// file, which no naming reads.
    const CRATES: &str = r#"
//- /main.rs crate:main deps:a,b
pub fn roots() {}
//- /a.rs crate:a deps:c
pub fn a() {}
//- /b.rs crate:b
pub fn b() {}
//- /c/lib.rs crate:c
mod inner;
pub fn c() {}
//- /c/inner.rs
pub fn inner() {}
//- /x.rs crate:x
pub fn x() {}
"#;

    /// What a build calls each file: a test's table.
    type Names = Vec<(FileId, String)>;

    /// One change of a table of names.
    #[derive(Debug, Clone, Copy)]
    enum Change {
        /// The root of the crate at this index of all crates takes another name.
        Rename(usize),
        /// The root of the crate at this index has no name.
        Unnamed(usize),
        /// `c`'s second file takes another name.
        Inner,
    }

    /// The naming of `crates` under `names`, by a front end whose hashes name files by them.
    fn naming(db: &base_db::Db, crates: &[Crate], names: &Names) -> Option<blaze_ids::ContentId> {
        let names = names.clone(); // clone: the table, owned by the front end's naming
        // clone: the name asked for, which the hashes own
        let named = move |file: FileId| names.iter().find(|it| it.0 == file).map(|it| it.1.clone());
        let frontend = HirFrontend::new(
            Databases::new(db),
            HashSource::Items(ItemHashes::new(Box::new(named))),
            BuildTime::default(),
        );
        frontend.with(|db| frontend.naming_over(db, crates))
    }

    /// The oracle: under `change`, `naming` moves when the change names a root of `closure`
    /// (a renamed one: another naming; an unnamed one: none) and stays the same for anything
    /// else (a crate outside the closure, a file that is not a root).
    fn held(
        db: &base_db::Db,
        all: &[Crate],
        closure: &[Crate],
        inner: FileId,
        before: &Names,
        change: Change,
        naming: &dyn Fn(&Names) -> Option<blaze_ids::ContentId>,
    ) -> Result<(), String> {
        let mut after = before.clone(); // clone: the table changed beside the one before
        let file = match change {
            Change::Rename(at) | Change::Unnamed(at) => all[at].data(db).root_file_id,
            Change::Inner => inner,
        };
        let at = after.iter().position(|it| it.0 == file).expect("every file has a name before");
        match change {
            Change::Unnamed(_) => {
                after.remove(at);
            }
            Change::Rename(_) | Change::Inner => after[at].1.push_str("/moved"),
        }
        let (was, now) = (naming(before), naming(&after));
        let of_closure = match change {
            Change::Rename(at) | Change::Unnamed(at) => closure.contains(&all[at]),
            Change::Inner => false,
        };
        let right = match (change, of_closure) {
            (Change::Unnamed(_), true) => was.is_some() && now.is_none(),
            (Change::Rename(_), true) => was.is_some() && now.is_some() && was != now,
            (_, false) => was.is_some() && was == now,
            (Change::Inner, true) => unreachable!("an inner change is of no closure (`of_closure` above)"),
        };
        match right {
            true => Ok(()),
            false => Err(format!(
                "{change:?} (of a crate the home loads: {of_closure}) took the naming from {was:?} to {now:?}"
            )),
        }
    }

    /// The invariant: a naming moves with the name of the root file of the home or of any
    /// crate it loads, directly or not, is `None` when one of those roots has no name, and
    /// moves with nothing else: not with a crate the home does not load, not with a file that
    /// is not a root. Its control: a naming of the home alone (the dependencies left out)
    /// is caught by renaming a dependency's root.
    #[test]
    fn a_naming_moves_with_a_root_files_name_and_with_nothing_else() {
        let fixture: &'static fixture::Fixture = Box::leak(Box::new(fixture::db(CRATES)));
        let db = fixture.db();
        let all: Vec<Crate> = all_crates(db).iter().copied().collect();
        let named = |name: &str| -> Crate {
            *all.iter()
                .find(|it| {
                    it.extra_data(db)
                        .display_name
                        .as_ref()
                        .is_some_and(|it| it.crate_name().as_str() == name)
                })
                .unwrap_or_else(|| panic!("no crate `{name}`"))
        };
        let (main, a, x) = (named("main"), named("a"), named("x"));
        let closure: &[Crate] = main.transitive_deps(db);
        assert!(
            closure.contains(&named("c")) && !closure.contains(&x),
            "the fixture's premise: `main` loads `c` through `a` and does not load `x`"
        );
        let inner = fixture.file_at("/c/inner.rs").file_id(db);
        // A fixture's crates are read from no image: no instance of them has a naming.
        let frontend = HirFrontend::new(
            Databases::new(db),
            HashSource::Items(ItemHashes::new(Box::new(|_: FileId| Some("named".to_owned())))),
            BuildTime::default(),
        );
        let root = frontend.root(hir_def::test_items::function(db, main, "roots"));
        assert_eq!(frontend.naming_of(&root), None, "an instance whose home is no image has no naming");

        let mut files: Vec<FileId> = all.iter().map(|it| it.data(db).root_file_id).collect();
        files.push(inner);
        let real = |names: &Names| naming(db, closure, names);
        let cases =
            std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(200);
        Hegel::new(|tc: TestCase| {
            let words = vec!["src/lib.rs", "src/main.rs", "lib.rs", "/r/a-1.0/src/lib.rs", ""];
            let before: Names = files
                .iter()
                // clone: the few words each draw samples from
                .map(|&file| (file, tc.draw(gs::sampled_from(words.clone())).to_owned()))
                .collect();
            let at = tc.draw(gs::integers::<u64>().max_value(all.len() as u64 - 1)) as usize;
            let change = match tc.draw(gs::integers::<u8>().max_value(2)) {
                0 => Change::Rename(at),
                1 => Change::Unnamed(at),
                _ => Change::Inner,
            };
            if let Err(why) = held(db, &all, closure, inner, &before, change, &real) {
                panic!("{why}");
            }
        })
        .settings(Settings::new().test_cases(cases).database(None))
        .run();

        // The control: the home alone. Renaming `a`'s root does not move it, which the
        // oracle names.
        let planted = |names: &Names| naming(db, &[main], names);
        let before: Names = files.iter().map(|&file| (file, "src/lib.rs".to_owned())).collect();
        let at = all.iter().position(|it| *it == a).expect("`a` is a crate");
        assert!(
            held(db, &all, closure, inner, &before, Change::Rename(at), &planted).is_err(),
            "a naming that leaves the dependencies out passed the oracle"
        );
        assert!(held(db, &all, closure, inner, &before, Change::Rename(at), &real).is_ok());
    }

    /// The crate of `db` named `name`.
    fn crate_named(db: &base_db::Db, name: &str) -> Crate {
        all_crates(db)
            .iter()
            .copied()
            .find(|it| {
                it.extra_data(db)
                    .display_name
                    .as_ref()
                    .is_some_and(|it| it.crate_name().as_str() == name)
            })
            .unwrap_or_else(|| panic!("no crate `{name}`"))
    }

    /// A front end over `db` whose hashes name no file: an ask reads none.
    fn over(db: &base_db::Db) -> HirFrontend<'_> {
        HirFrontend::new(
            Databases::new(db),
            HashSource::Items(ItemHashes::new(Box::new(|_: FileId| None))),
            BuildTime::default(),
        )
    }

    /// The asks' programs, up to the body of `roots`: `dep`'s generics, a closure it calls, a
    /// static it reads, a method of a generic type, and `main`'s own generic over `dep`'s.
    const ASKS: &str = r#"
//- minicore: sized, fn
//- /dep.rs crate:dep
pub struct Wrap<T>(pub T);
impl<T> Wrap<T> {
    pub fn get(self) -> T { self.0 }
}
pub fn id<T>(x: T) -> T { x }
pub fn pair<A, B>(a: A, b: B) -> (A, B) { (a, b) }
pub fn call<F: FnOnce() -> u32>(f: F) -> u32 { f() }
pub static COUNT: u32 = 7;
pub fn count() -> u32 { COUNT }
//- /main.rs crate:main deps:dep
pub struct Local(pub u8);
pub fn local<T>(x: T) -> T { dep::id(x) }
pub fn roots() {
"#;

    /// What `roots` calls, `{t}` a type and `{v}` a value of it.
    const CALLS: [&str; 6] = [
        "dep::id::<{t}>({v})",
        "dep::pair::<{t}, u8>({v}, 1u8)",
        "dep::call(|| 3u32)",
        "dep::count()",
        "dep::Wrap::<{t}>({v}).get()",
        "local::<{t}>({v})",
    ];

    /// The types the calls are instantiated with, each with a value: primitives, a tuple, an
    /// array, `dep`'s generic over a primitive and `main`'s own type.
    const TYPES: [(&str, &str); 7] = [
        ("u8", "7u8"),
        ("u32", "7u32"),
        ("bool", "true"),
        ("(u8, bool)", "(7u8, true)"),
        ("[u16; 3]", "[7u16, 8, 9]"),
        ("dep::Wrap<u32>", "dep::Wrap(7u32)"),
        ("Local", "Local(7)"),
    ];

    /// A program of `calls`, each a call of [`CALLS`] with a type of [`TYPES`].
    fn program(calls: &[(usize, usize)]) -> String {
        // alloc: a test's source
        let mut text = ASKS.to_owned();
        for &(call, ty) in calls {
            let (ty, value) = TYPES[ty];
            text.push_str("    let _ = ");
            text.push_str(&CALLS[call].replace("{t}", ty).replace("{v}", value));
            text.push_str(";\n");
        }
        text.push_str("}\n");
        text
    }

    /// The origins an ask is drawn with: every tag [`super::put_origin`] writes.
    fn origins() -> [Origin; 4] {
        [
            Origin::Std,
            Origin::Dependency(Package::new("dep")),
            Origin::BuildTime(Package::new("build")),
            Origin::Member(Package::new("main")),
        ]
    }

    /// `root` and every function it reaches, each once, in the order first reached, up to a
    /// bound well above what the programs reach.
    fn reached(frontend: &HirFrontend<'_>, root: HirInstance) -> Vec<HirInstance> {
        frontend.with(|db| {
            let mut seen = std::collections::HashSet::new();
            let mut next = vec![root];
            let mut all = Vec::new();
            while let Some(instance) = next.pop() {
                assert!(all.len() < 512, "the program reaches more than 512 functions");
                // clone: the test's set of what it reached
                if !seen.insert(instance.clone()) {
                    continue;
                }
                if let Kind::Code(..) = &instance.0 {
                    let refs = refs::walk(db, &instance, refs::Walking::plain(false))
                        .unwrap_or_else(|why| panic!("the walk of {instance:?}: {why}"));
                    next.extend(refs.into_iter().rev().filter_map(|it| match it {
                        FrontRef::Function(callee) => Some(callee),
                        _ => None,
                    }));
                }
                all.push(instance);
            }
            all
        })
    }

    /// Why `instance` has no portable form, where it has none: the allocator shim's, an
    /// environment that is not empty, or (written over every crate of the database, so only
    /// its home's closure refused it) an item outside its home's closure.
    fn unported(
        frontend: &HirFrontend<'_>,
        db: &base_db::Db,
        instance: &HirInstance,
    ) -> Option<&'static str> {
        let every: Vec<Crate> = all_crates(db).iter().copied().collect();
        frontend.with(|_| match &instance.0 {
            Kind::Allocator(_) => Some("the allocator shim's"),
            Kind::Code(_, env) if env.param_env(db) != ParamEnv::empty() => {
                Some("of an environment that is not empty")
            }
            _ => {
                let closure = Closure { db, crates: &every };
                let mut writer = port::Writer::new(db, DbInterner::new_no_crate(db));
                let mut out = Vec::new();
                put_instance(&closure, &mut writer, &mut out, instance)?;
                let ported = writer.finish(&mut |item| closure.name(item));
                ported.ok().map(|_| "naming an item outside its home's closure")
            }
        })
    }

    /// The oracle of an ask: `write` gives `asked` bytes that read back in its own database
    /// (`first`) as `asked` and in a second database of the same sources (`second`) as an
    /// instance whose ask is those bytes again; where it gives none, the instance has no
    /// portable form ([`unported`]).
    fn held_ask(
        first: &HirFrontend<'_>,
        db: &base_db::Db,
        second: &HirFrontend<'_>,
        asked: &(HirInstance, Origin),
        write: &dyn Fn(&(HirInstance, Origin)) -> Option<Vec<u8>>,
    ) -> Result<(), String> {
        let Some(bytes) = write(asked) else {
            return match unported(first, db, &asked.0) {
                Some(_) => Ok(()),
                None => Err(format!(
                    "{asked:?} has no ask, and it is not the allocator shim's, of an environment \
                     that is not empty, nor naming an item outside its home's closure"
                )),
            };
        };
        match first.ask_of(&bytes) {
            Ok(back) if back == *asked => {}
            other => return Err(format!("{asked:?} read back in its own database as {other:?}")),
        }
        let back = second.ask_of(&bytes).map_err(|why| {
            format!("{asked:?} does not read in a second database of the same sources: {why}")
        })?;
        match second.ask_bytes(&back) {
            Some(again) if again == bytes => Ok(()),
            again => Err(format!(
                "{asked:?} read in a second database as {back:?}, whose ask is {again:?}, not \
                 the {} bytes it was read from",
                bytes.len()
            )),
        }
    }

    /// `instance` with its generic arguments as none: the planted writer's.
    fn without_args(frontend: &HirFrontend<'_>, instance: &HirInstance) -> HirInstance {
        frontend.with(|_| match &instance.0 {
            Kind::Code(code, env) => HirInstance(Kind::Code(
                Instance { args: GenericArgs::new_from_slice(&[]).store(), ..*code },
                env.clone(), // clone: the planted instance's own environment
            )),
            _ => instance.clone(), // clone: the planted instance, as it is
        })
    }

    /// The invariant: for every root and callee of the generated programs (`dep`'s generics
    /// over primitives, tuples, arrays, `dep`'s and `main`'s types, a closure, a static's
    /// reader, a method of a generic type), asked for by any origin, `ask_of(ask_bytes(a)) ==
    /// a` in its own database, and in a second database of the same sources the ask reads as
    /// an instance whose ask is the same bytes; where `ask_bytes` is `None` the instance is the
    /// allocator shim's, has an environment that is not empty, or names an item outside its
    /// home's closure. An ask of another magic, with bytes left over, of origin tag 0 or
    /// naming an item the database has none of is refused by name. Its control: an ask
    /// written with the instance's arguments as none, caught by a generic callee.
    #[test]
    fn an_ask_reads_back_as_the_instance_it_was_written_of() {
        let cases =
            std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(40);
        Hegel::new(|tc: TestCase| {
            let count: usize = tc.draw(gs::integers().min_value(1).max_value(5));
            let calls: Vec<(usize, usize)> = (0..count)
                .map(|_| {
                    let call = tc.draw(gs::integers::<usize>().max_value(CALLS.len() - 1));
                    (call, tc.draw(gs::integers::<usize>().max_value(TYPES.len() - 1)))
                })
                .collect();
            let text = program(&calls);
            let (one, two) = (fixture::db(&text), fixture::db(&text));
            let (first, second) = (over(one.db()), over(two.db()));
            let main = crate_named(one.db(), "main");
            let root = first.root(hir_def::test_items::function(one.db(), main, "roots"));
            let write = |asked: &(HirInstance, Origin)| first.ask_bytes(asked);
            for instance in reached(&first, root) {
                let origin = origins()[tc.draw(gs::integers::<usize>().max_value(3))];
                if let Err(why) = held_ask(&first, one.db(), &second, &(instance, origin), &write) {
                    panic!("{why}\n{text}");
                }
            }
        })
        .settings(Settings::new().test_cases(cases).database(None))
        .run();

        let text = program(&[(0, 1), (5, 6)]);
        let (one, two) = (fixture::db(&text), fixture::db(&text));
        let (first, second) = (over(one.db()), over(two.db()));
        let (main, dep) = (crate_named(one.db(), "main"), crate_named(one.db(), "dep"));
        let root = first.root(hir_def::test_items::function(one.db(), main, "roots"));
        let all = reached(&first, root.clone()); // clone: the root, asked for below too
        let real = |asked: &(HirInstance, Origin)| first.ask_bytes(asked);
        for instance in &all {
            // clone: the ask's instance, which the oracle owns
            let asked = (instance.clone(), Origin::Std);
            held_ask(&first, one.db(), &second, &asked, &real).unwrap_or_else(|why| panic!("{why}"));
        }

        // The control: the arguments written as none. `dep::id::<u32>` reads back as
        // another instance, which the oracle names.
        let planted = |asked: &(HirInstance, Origin)| {
            first.ask_bytes(&(without_args(&first, &asked.0), asked.1))
        };
        let id = hir_def::test_items::function(one.db(), dep, "id");
        let generic = all.iter().find(|it| it.function() == Some(id)).expect("`roots` calls `dep::id`");
        // clone: the ask's instance, which the oracle owns
        let asked = (generic.clone(), Origin::Std);
        assert!(
            held_ask(&first, one.db(), &second, &asked, &planted).is_err(),
            "an ask written with its arguments as none passed the oracle"
        );

        // Refused by name: another magic, a byte left over, origin tag 0, an item the
        // database has none of.
        let bytes = first.ask_bytes(&(root, Origin::Std)).expect("`roots` has an ask");
        let mut magic = bytes.clone(); // clone: the ask, changed beside it
        magic[7] = b'0';
        let why = first.ask_of(&magic).expect_err("an ask of another magic read");
        assert!(why.contains("bzask001"), "{why}");
        let mut longer = bytes.clone(); // clone: the ask, changed beside it
        longer.push(0);
        assert!(first.ask_of(&longer).is_err(), "an ask with a byte left over read");
        let ported = u32::from_le_bytes(bytes[8..12].try_into().expect("four bytes")) as usize;
        let mut requester = bytes.clone(); // clone: the ask, changed beside it
        requester[12 + ported] = 0;
        let why = first.ask_of(&requester).expect_err("an ask of origin tag 0 read");
        assert!(why.contains("tag 0"), "{why}");
        let renamed = text.replace("pub fn id<T>", "pub fn ident<T>").replace("dep::id(", "dep::ident(");
        let renamed = fixture::db(&renamed.replace("dep::id::<", "dep::ident::<"));
        let id_ask = first.ask_bytes(&asked).expect("`dep::id::<u32>` has an ask");
        let why = over(renamed.db()).ask_of(&id_ask).expect_err("an ask of an item it lacks read");
        assert!(why.contains("none of"), "{why}");
    }

    /// The names of a static key's inputs, in [`static_key_of`]'s order.
    const STATIC_INPUTS: [&str; 5] =
        ["producer", "stage", "image key of its crate", "item key", "instance"];

    /// A static's key over `inputs` ([`super::static_key_over`]).
    fn static_key_of(inputs: &[[u8; 32]; 5]) -> blaze_cas::Key {
        let id = (ItemKey(ContentId(inputs[3])), ContentId(inputs[4]));
        let [producer, stage, image] = [inputs[0], inputs[1], inputs[2]].map(ContentId);
        static_key_over(producer, &stage, &image, &id)
    }

    /// The oracle of a static's key: under `change` (one input's byte flipped, or none) `key`
    /// moves exactly when an input changed.
    fn held_static(
        before: &[[u8; 32]; 5],
        change: Option<(usize, usize)>,
        key: &dyn Fn(&[[u8; 32]; 5]) -> blaze_cas::Key,
    ) -> Result<(), String> {
        let mut after = *before;
        if let Some((input, byte)) = change {
            after[input][byte] ^= 1;
        }
        match (change, key(before) == key(&after)) {
            (None, true) | (Some(_), false) => Ok(()),
            (None, false) => Err("the same inputs gave two keys".to_owned()),
            (Some((input, byte)), true) => Err(format!(
                "byte {byte} of the {} changed and the key stayed as it was",
                STATIC_INPUTS[input]
            )),
        }
    }

    /// The invariant: a static's key moves with each of its inputs (the producer, the stage,
    /// the image key of its crate, its item key and which instance of it) and with nothing
    /// else, so one record is served wherever they are the same and never where one differs;
    /// a static of a crate with no image (a fixture's, as a workspace crate's) has no key.
    /// Its control: a key that leaves the image key out, caught by one dependency at two
    /// contents under one name and version (a git dependency at two commits): one item key,
    /// two images, which would serve one version's static to the other.
    #[test]
    fn a_static_key_moves_with_its_crates_image_and_with_nothing_else() {
        let cases =
            std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(300);
        Hegel::new(|tc: TestCase| {
            // A few values a byte, so inputs often equal each other: each is told apart by
            // its place.
            let mut inputs = [[0u8; 32]; 5];
            for input in &mut inputs {
                *input = [tc.draw(gs::integers::<u8>().max_value(2)); 32];
            }
            let change = match tc.draw(gs::integers::<usize>().max_value(STATIC_INPUTS.len())) {
                input if input == STATIC_INPUTS.len() => None,
                input => Some((input, tc.draw(gs::integers::<usize>().max_value(31)))),
            };
            if let Err(why) = held_static(&inputs, change, &static_key_of) {
                panic!("{why}");
            }
        })
        .settings(Settings::new().test_cases(cases).database(None))
        .run();

        let fixture = fixture::db(&program(&[(3, 0)]));
        let db = fixture.db();
        let count = hir_def::test_items::static_item(db, crate_named(db, "dep"), "COUNT");
        assert_eq!(
            over(db).with(|db| super::static_image(db, &HirStatic(count))),
            None,
            "a static of a crate read from no image has no image key, so no key"
        );

        // The control: the image key left out. Two contents of one dependency differ in the
        // image key alone, and the planted key is one for both.
        let planted = |inputs: &[[u8; 32]; 5]| {
            let mut left_out = *inputs;
            left_out[2] = [0; 32];
            static_key_of(&left_out)
        };
        let versions = [[1u8; 32], [2; 32], [3; 32], [4; 32], [5; 32]];
        assert!(held_static(&versions, Some((2, 0)), &static_key_of).is_ok());
        assert!(
            held_static(&versions, Some((2, 0)), &planted).is_err(),
            "a static key that leaves the image key out passed the oracle"
        );
    }
}
