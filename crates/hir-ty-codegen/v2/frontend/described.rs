//! A function's describe as a store record (docs/v2/plan/design/store.md, "Describe
//! records"): its key, made of content before the describe runs, and its bytes, which name
//! every instance and static by content.
//!
//! A describe was a memo of one database ([`described_memo`]: its key holds interned ids and
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
//! the database's memo's, as before.
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
            bound,
            cold,
        } = described;
        put_id(&mut out, &item.0);
        put_id(&mut out, &code.0);
        put_id(&mut out, &template_code.0);
        put_id(&mut out, &built_in_code.0);
        put_id(&mut out, args);
        put_id(&mut out, identity);
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
                    // clone: the query's key by value, a few `Copy` ids in a `Clone`-only type
                    put_counted(&mut out, &counted_memo(db, callee.clone(), ()));
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
