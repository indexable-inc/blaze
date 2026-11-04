//! Component registration (port of `registerComponent` / `registerOpaque` / `requireComponent` in ecs.zig): the world knows
//! components BY NAME; each component entity carries `mc.LayoutHash` (u64), and a layout hash that differs from the one a
//! module was built against is refused before flecs can abort on a size mismatch.
use crate::sys;
use crate::untyped::UntypedWorld;
use crate::world::{Id, World, cs};
use core::ffi::c_void;
use core::fmt;
#[cfg(feature = "meta")]
use hyperion_layout::layout::{EnumKind, Prim, Recipe};
use hyperion_layout::Layout;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The component exists with another layout: the host must migrate and remove it first.
    LayoutChanged {
        name: String,
        world: Option<u64>,
        built_against: u64,
    },
    NotAComponent(String),
    RegisterFailed(String),
    /// A typed query or typed access that cannot be built (an aliasing pair of fields, a term that needs a missing component).
    Query(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::LayoutChanged { name, world, built_against } => write!(
                f,
                "component '{name}' layout mismatch (world hash {}, built against {built_against:x}); rebuild",
                world.map(|h| format!("{h:x}")).unwrap_or_else(|| "none".into())
            ),
            Error::NotAComponent(n) => write!(f, "component '{n}' is not registered (is its provider module loaded?)"),
            Error::RegisterFailed(n) => write!(f, "registering component '{n}' failed"),
            Error::Query(m) => write!(f, "typed query: {m}"),
        }
    }
}
impl std::error::Error for Error {}

pub const LAYOUT_HASH_NAME: &str = "mc.LayoutHash";
const TYPE_SCOPE: &str = "types.";

pub fn layout_hash_component(w: World) -> Id {
    let id = w.lookup(LAYOUT_HASH_NAME);
    if id != 0 {
        return id;
    }
    let e = w.ensure_entity(LAYOUT_HASH_NAME);
    // SAFETY: zeroed desc; the entity is ours.
    unsafe {
        let mut d: sys::ecs_component_desc_t = core::mem::zeroed();
        d.entity = e;
        d.type_.size = 8;
        d.type_.alignment = 8;
        let id = sys::ecs_component_init(w.as_ptr(), &d);
        // reflected as `{ hash: u64 }` (same 8 bytes) so the console shows every component's layout hash by name
        #[cfg(feature = "meta")]
        {
            let mut sd: sys::ecs_struct_desc_t = core::mem::zeroed();
            sd.entity = id;
            let name = cs("hash");
            sd.members[0].name = name.as_ptr();
            sd.members[0].type_ = sys::FLECS_IDecs_u64_tID_;
            sd.members[0].offset = 0;
            sd.members[0].use_offset = true;
            sys::ecs_struct_init(w.as_ptr(), &sd);
        }
        id
    }
}

fn init_component(w: World, name: &str, size: usize, align: usize, hooks: Option<&sys::ecs_type_hooks_t>) -> Id {
    let e = w.ensure_entity(name);
    // SAFETY: zeroed desc; sizes come from a repr(C) type.
    unsafe {
        let mut d: sys::ecs_component_desc_t = core::mem::zeroed();
        d.entity = e;
        d.type_.size = size as i32;
        // A zero-sized type is a tag: flecs aborts on size 0 with a nonzero alignment (Rust's ZST alignment is 1), so a tag gets alignment 0.
        d.type_.alignment = if size == 0 { 0 } else { align as i32 };
        if let Some(h) = hooks.filter(|_| size != 0) {
            d.type_.hooks = *h;
        }
        sys::ecs_component_init(w.as_ptr(), &d)
    }
}

/// No module registers flecs lifecycle hooks (ctor, dtor, copy, move): components are `Copy` plain data, so none are needed, and a hook is a function
/// pointer into the image that set it (docs/plain-abi-plan.md section 1). Checked on every registration; `on_add`/`on_set` style hooks are rule 29g's business.
/// One hook is always there and allowed: flecs' own zeroing `ctor` (`flecs_default_ctor`, set for every reflected struct by its runtime-type defaults; a pointer
/// into the host's flecs, never into a module image).
fn hook_free(w: World, id: Id) -> bool {
    // SAFETY: live world; `ecs_get_hooks_id` returns null or a pointer into the type info, read at once.
    unsafe {
        let h = sys::ecs_get_hooks_id(w.as_ptr(), id);
        h.is_null() || {
            let h = &*h;
            h.ctor.is_none_or(|c| c as usize == sys::flecs_default_ctor as *const () as usize)
                && h.dtor.is_none()
                && h.copy.is_none()
                && h.move_.is_none()
                && h.copy_ctor.is_none()
                && h.move_ctor.is_none()
                && h.ctor_move_dtor.is_none()
                && h.move_dtor.is_none()
        }
    }
}

#[cfg(feature = "meta")]
fn prim_id(k: Prim) -> Id {
    // SAFETY: flecs primitive type ids are immutable globals.
    unsafe {
        match k {
            Prim::Bool => sys::FLECS_IDecs_bool_tID_,
            Prim::U8 => sys::FLECS_IDecs_u8_tID_,
            Prim::U16 => sys::FLECS_IDecs_u16_tID_,
            Prim::U32 => sys::FLECS_IDecs_u32_tID_,
            Prim::U64 => sys::FLECS_IDecs_u64_tID_,
            // Pointers are never exposed as members (see `is_hidden`); `uptr` is what a tool sees if one is ever forced through.
            Prim::Ptr => sys::FLECS_IDecs_uptr_tID_,
            Prim::Str => sys::FLECS_IDecs_string_tID_,
            Prim::I8 => sys::FLECS_IDecs_i8_tID_,
            Prim::I16 => sys::FLECS_IDecs_i16_tID_,
            Prim::I32 => sys::FLECS_IDecs_i32_tID_,
            Prim::I64 => sys::FLECS_IDecs_i64_tID_,
            Prim::F32 => sys::FLECS_IDecs_f32_tID_,
            Prim::F64 => sys::FLECS_IDecs_f64_tID_,
        }
    }
}

#[cfg(feature = "meta")]
fn prim_of(r: &Recipe) -> Option<Prim> {
    match r {
        Recipe::Prim { kind, .. } => Some(*kind),
        _ => None,
    }
}

#[cfg(feature = "meta")]
struct MemberType {
    ty: Id,
    count: i32,
}

#[cfg(feature = "meta")]
fn hyperion_log(msg: &str) {
    crate::hooks::log(2, msg);
}

#[cfg(feature = "meta")]
/// A fieldless enum's flecs type, made at `path` (or on `entity`, a registered component, when nonzero).
fn enum_type(w: World, path: &str, entity: Id, consts: &[(&'static str, i64)], tag: &Recipe) -> Option<Id> {
    let tag_prim = prim_of(tag)?;
    let signed = matches!(tag_prim, Prim::I8 | Prim::I16 | Prim::I32 | Prim::I64);
    if consts.len() > 32 {
        // flecs' enum desc holds 32 constants: reflect the underlying integer instead of failing the whole component.
        hyperion_log(&format!("{path}: more than 32 enum constants, reflected as its integer"));
        return Some(prim_id(tag_prim));
    }
    let pc = cs(path);
    let names: Vec<_> = consts.iter().map(|c| cs(c.0)).collect();
    // SAFETY: zeroed descs; flecs copies the constant names.
    unsafe {
        let mut d: sys::ecs_enum_desc_t = core::mem::zeroed();
        d.entity = if entity != 0 {
            entity
        } else {
            let mut ed: sys::ecs_entity_desc_t = core::mem::zeroed();
            ed.name = pc.as_ptr();
            sys::ecs_entity_init(w.as_ptr(), &ed)
        };
        d.underlying_type = prim_id(tag_prim);
        for (i, c) in consts.iter().enumerate() {
            d.constants[i].name = names[i].as_ptr();
            if signed { d.constants[i].value = c.1 } else { d.constants[i].value_unsigned = c.1 as u64 }
        }
        Some(sys::ecs_enum_init(w.as_ptr(), &d))
    }
}

#[cfg(feature = "meta")]
/// A named component entity of `size` and `align` (a nested struct must be one before `ecs_struct_init`, or flecs aborts "has 0
/// alignment"; an opaque type must have a size).
fn sized_entity(w: World, path: &str, size: usize, align: usize) -> Id {
    let pc = cs(path);
    // SAFETY: zeroed descs.
    unsafe {
        let mut ed: sys::ecs_entity_desc_t = core::mem::zeroed();
        ed.name = pc.as_ptr();
        let mut cd: sys::ecs_component_desc_t = core::mem::zeroed();
        cd.entity = sys::ecs_entity_init(w.as_ptr(), &ed);
        cd.type_.size = size as i32;
        cd.type_.alignment = align as i32;
        sys::ecs_component_init(w.as_ptr(), &cd)
    }
}

#[cfg(feature = "meta")]
/// A member's flecs type. A nested struct or enum is one type per shape wherever it is used ([`type_of_recipe`]): every
/// `Rgba` member of every component is the one `Rgba`, so a script hands a value of one component's member to another's
/// (`Edge: {dark: $tone.dark}`) and a whole `Fill` to a `Fill` member. A `server.*` singleton's nested struct keeps a type of
/// its own under `types.<component>.<member>`, since the settings pump names its members by that path. (Not under the
/// component itself: `<component>.<member>` is the member entity flecs creates; reusing it aborted in the Zig version.)
fn member_type(w: World, owner: &str, fname: &str, r: &'static Recipe) -> Option<MemberType> {
    match r {
        Recipe::Prim { kind, .. } => Some(MemberType { ty: prim_id(*kind), count: 0 }),
        Recipe::Array { len, elem, .. } => {
            let e = member_type(w, owner, fname, elem)?;
            // A nested array (`[[i32; 16]; 2]`) is flattened to one array member (`i32[32]`): the bytes are laid out identically.
            Some(MemberType { ty: e.ty, count: (*len as i32) * e.count.max(1) })
        }
        // SAFETY: flecs primitive type ids are immutable globals.
        Recipe::Enum { kind: EnumKind::Entity, .. } => Some(MemberType { ty: unsafe { sys::FLECS_IDecs_entity_tID_ }, count: 0 }),
        Recipe::Struct { size, align, .. } if owner.starts_with("server.") => {
            let ent = sized_entity(w, &format!("{TYPE_SCOPE}{owner}.{fname}"), *size, *align);
            let ty = register_struct(w, r, &format!("{owner}.{fname}"), ent);
            (ty != 0).then_some(MemberType { ty, count: 0 })
        }
        Recipe::Enum { .. } | Recipe::Struct { .. } | Recipe::Opaque { .. } => {
            let ty = type_of_recipe(w, r);
            (ty != 0).then_some(MemberType { ty, count: 0 })
        }
    }
}

#[cfg(feature = "meta")]
/// A key for what `r` reads as: its layout hash is of its shape only (`Shared<A>` and `Shared<B>` share one), so this walks
/// the forms too. Recipes cannot be recursive (a const cannot hold itself), so the walk ends.
fn reflect_key(r: &Recipe) -> u64 {
    use hyperion_layout::layout::Form;
    fn mix(h: u64, x: u64) -> u64 {
        (h ^ x).wrapping_mul(0x0100_0000_01b3).rotate_left(29)
    }
    fn text(h: u64, s: &str) -> u64 {
        s.bytes().fold(mix(h, s.len() as u64), |h, b| mix(h, u64::from(b)))
    }
    match r {
        Recipe::Opaque { shape, form, .. } => {
            let h = mix(0x6f70_6171_7565, reflect_key(shape));
            match form {
                Form::Text { .. } => mix(h, 1),
                Form::Json { .. } => mix(h, 2),
                Form::List { elem, .. } => mix(mix(h, 3), reflect_key(elem)),
                Form::Maybe { elem, .. } => mix(mix(h, 4), reflect_key(elem)),
                Form::Ref { elem, .. } => mix(mix(h, 5), reflect_key(elem)),
                Form::OneOf { variants, .. } => variants.iter().fold(mix(h, 6), |h, v| mix(text(h, v.name), v.data.map_or(0, reflect_key))),
            }
        }
        Recipe::Array { len, elem, .. } => mix(mix(0x6172_7261_79, *len as u64), reflect_key(elem)),
        Recipe::Struct { fields, .. } => fields.iter().fold(mix(0x7374_7275_6374, r.hash()), |h, f| mix(text(h, f.name), reflect_key(f.ty))),
        Recipe::Prim { .. } | Recipe::Enum { .. } => r.hash(),
    }
}

/// The relationship from a shape's entity under `types.` (`types.r<key>`) to the registered component that is that shape's
/// type: a component of that shape is the type every such member uses, registered before them ([`adopt_shape`]) or after
/// ([`claim_shape`]).
pub const REFLECTS_AS_NAME: &str = "mc.ReflectsAs";

#[cfg(feature = "meta")]
fn shape_path(key: u64) -> String {
    format!("{TYPE_SCOPE}r{key:016x}")
}

#[cfg(feature = "meta")]
/// The type made or adopted for the shape at `path`, or 0 when there is none yet. A `types.r<key>` that names a component gone
/// since (a plugin unloaded: flecs drops the pair with its target) is deleted, so the shape is made again.
fn shape_type(w: World, path: &str) -> Id {
    let have = w.lookup(path);
    if have == 0 {
        return 0;
    }
    let rel = w.lookup(REFLECTS_AS_NAME);
    let target = if rel == 0 { 0 } else { w.target(have, rel) };
    if target != 0 {
        return target;
    }
    // SAFETY: live world; `have` is a live entity.
    if unsafe { sys::ecs_get_type_info(w.as_ptr(), have).is_null() } {
        unsafe { sys::ecs_delete(w.as_ptr(), have) };
        return 0;
    }
    have
}

#[cfg(feature = "meta")]
/// Make the component `id`, just reflected from `r`, the type of its shape, unless the shape has one already.
fn adopt_shape(w: World, r: &'static Recipe, id: Id) {
    // SAFETY: live world; an immutable flecs global. A component every member of which is hidden has no type to share.
    if !unsafe { sys::ecs_has_id(w.as_ptr(), id, sys::FLECS_IDEcsTypeID_) } {
        return;
    }
    let path = shape_path(reflect_key(r));
    if shape_type(w, &path) != 0 {
        return;
    }
    let alias = w.ensure_entity(&path);
    w.add_id(alias, crate::world::pair(w.ensure_entity(REFLECTS_AS_NAME), id));
}

#[cfg(feature = "meta")]
/// The bare type a member made for `T`'s shape before `T` was registered, renamed `name` to be `T` itself, with
/// `types.r<key>` left naming it as [`adopt_shape`] would have. A shape is then one flecs type whichever registers first, the
/// component or a member of its shape, so a script's `Rgba: {…}` constant goes into every member of that shape. Plain
/// structs only (what scripts hand from one component to another), and not a `server.*` singleton, whose nested members
/// keep types of their own. 0 when there is none to take: the component is made as usual.
fn claim_shape<T: Layout>(w: World, name: &str) -> Id {
    if size_of::<T>() == 0 || name.starts_with("server.") || !matches!(T::RECIPE, Recipe::Struct { .. }) {
        return 0;
    }
    let path = shape_path(reflect_key(T::RECIPE));
    let have = w.lookup(&path);
    if have == 0 {
        return 0;
    }
    let rel = w.lookup(REFLECTS_AS_NAME);
    if rel != 0 && w.target(have, rel) != 0 {
        return 0;
    }
    // SAFETY: live world; `have` is a live entity and its type info is read at once.
    let fits = unsafe {
        let ti = sys::ecs_get_type_info(w.as_ptr(), have);
        !ti.is_null() && (*ti).size as usize == size_of::<T>() && (*ti).alignment as usize == align_of::<T>()
    };
    if !fits {
        return 0;
    }
    // SAFETY: an immutable flecs global.
    let types = w.target(have, unsafe { sys::EcsChildOf });
    let n = cs(name);
    // SAFETY: live world and entity; the name outlives the call (flecs copies it). With its old name gone, naming it by
    // `ecs_entity_init` resolves `name` (its dots, the current scope) as `init_component` would for a new entity.
    unsafe {
        sys::ecs_set_name(w.as_ptr(), have, core::ptr::null());
        if types != 0 {
            sys::ecs_remove_id(w.as_ptr(), have, crate::world::pair(sys::EcsChildOf, types));
        }
        let mut d: sys::ecs_entity_desc_t = core::mem::zeroed();
        d.id = have;
        d.name = n.as_ptr();
        sys::ecs_entity_init(w.as_ptr(), &d);
    }
    let alias = w.ensure_entity(&path);
    w.add_id(alias, crate::world::pair(w.ensure_entity(REFLECTS_AS_NAME), have));
    have
}

#[cfg(feature = "meta")]
/// The flecs type of `r` wherever a type stands alone (a member, an opaque type, its element, a choice's data): a primitive's
/// own id, else one per shape: the component adopted for it ([`REFLECTS_AS_NAME`]) or an entity under `types.` named by its
/// [`reflect_key`], made the first time and shared by every use. 0: not reflectable.
pub fn type_of_recipe(w: World, r: &'static Recipe) -> Id {
    match r {
        Recipe::Prim { kind: Prim::Ptr, .. } | Recipe::Array { len: 0, .. } => 0,
        Recipe::Prim { kind, .. } => prim_id(*kind),
        // SAFETY: an immutable flecs global.
        Recipe::Enum { kind: EnumKind::Entity, .. } => unsafe { sys::FLECS_IDecs_entity_tID_ },
        _ => {
            let path = shape_path(reflect_key(r));
            let have = shape_type(w, &path);
            if have != 0 {
                return have;
            }
            match r {
                Recipe::Enum { consts, tag, .. } => enum_type(w, &path, 0, consts, tag).unwrap_or(0),
                Recipe::Struct { size, align, .. } => {
                    let ent = sized_entity(w, &path, *size, *align);
                    register_struct(w, r, &format!("m{:016x}", reflect_key(r)), ent)
                }
                Recipe::Array { len, elem, .. } => {
                    let e = type_of_recipe(w, elem);
                    if e == 0 {
                        return 0;
                    }
                    let pc = cs(&path);
                    // SAFETY: zeroed descs; the name outlives the call.
                    unsafe {
                        let mut ed: sys::ecs_entity_desc_t = core::mem::zeroed();
                        ed.name = pc.as_ptr();
                        let d = sys::ecs_array_desc_t { entity: sys::ecs_entity_init(w.as_ptr(), &ed), type_: e, count: *len as i32 };
                        sys::ecs_array_init(w.as_ptr(), &d)
                    }
                }
                Recipe::Opaque { .. } => opaque_type(w, &path, 0, r),
                Recipe::Prim { .. } => 0,
            }
        }
    }
}

#[cfg(feature = "meta")]
/// The marks den's schema reads off an opaque type: what flecs's own type info cannot say (its `as_type` reads the same for a
/// vector and an optional value, and for words and JSON).
pub mod mark {
    /// An opaque type that is a value or none: its `as_type` is a vector of at most one.
    pub const OPTIONAL: &str = "mc.Optional";
    /// An opaque type that is one of several variants: its `as_type` is a struct with a member per variant.
    pub const CHOICE: &str = "mc.Choice";
    /// An opaque type whose words are JSON.
    pub const JSON: &str = "mc.Json";
}

#[cfg(feature = "meta")]
/// An opaque type for `r` (a [`Recipe::Opaque`]): at `path`, or on `entity` (a registered component) when nonzero. 0 when what it
/// reads as is not reflectable.
fn opaque_type(w: World, path: &str, entity: Id, r: &'static Recipe) -> Id {
    use hyperion_layout::layout::Form;
    let Recipe::Opaque { form, serialize, .. } = r else { return 0 };
    hyperion_layout::layout::set_serializer(serialize_recipe);
    let hash = reflect_key(r);
    // SAFETY: an immutable flecs global.
    let string = unsafe { sys::FLECS_IDecs_string_tID_ };
    let (as_type, marked) = match form {
        Form::Text { .. } => (string, None),
        Form::Json { .. } => (string, Some(mark::JSON)),
        Form::List { elem, .. } | Form::Maybe { elem, .. } => {
            let e = type_of_recipe(w, elem);
            if e == 0 {
                return 0;
            }
            let pc = cs(&format!("{TYPE_SCOPE}v{hash:016x}"));
            // SAFETY: zeroed descs; the name outlives the call.
            let v = unsafe {
                let mut ed: sys::ecs_entity_desc_t = core::mem::zeroed();
                ed.name = pc.as_ptr();
                let d = sys::ecs_vector_desc_t { entity: sys::ecs_entity_init(w.as_ptr(), &ed), type_: e };
                sys::ecs_vector_init(w.as_ptr(), &d)
            };
            (v, matches!(form, Form::Maybe { .. }).then_some(mark::OPTIONAL))
        }
        Form::OneOf { variants, .. } => (choice_type(w, &format!("{TYPE_SCOPE}c{hash:016x}"), variants), Some(mark::CHOICE)),
        Form::Ref { elem, .. } => match elem {
            // What an opaque value holds reads as that value would (its as_type and marks), through its own form.
            Recipe::Opaque { .. } => {
                let inner = type_of_recipe(w, elem);
                // SAFETY: live world; `inner` is 0 or an opaque type made above.
                let Some(o) = (inner != 0).then(|| unsafe { sys::ecs_get_id(w.as_ptr(), inner, sys::FLECS_IDEcsOpaqueID_).cast::<sys::EcsOpaque>().as_ref() }).flatten() else {
                    return 0;
                };
                let m = [mark::OPTIONAL, mark::CHOICE, mark::JSON].into_iter().find(|m| {
                    let id = w.lookup(m);
                    // SAFETY: live world.
                    id != 0 && unsafe { sys::ecs_has_id(w.as_ptr(), inner, id) }
                });
                (o.as_type, m)
            }
            Recipe::Struct { fields, .. } => {
                // Serialized member by member, each as its own type: make them now, so serializing only looks them up.
                if fields.iter().filter(|f| !is_hidden(f.ty)).any(|f| type_of_recipe(w, f.ty) == 0) {
                    return 0;
                }
                (type_of_recipe(w, elem), None)
            }
            _ => (type_of_recipe(w, elem), None),
        },
    };
    if as_type == 0 {
        return 0;
    }
    let ent = if entity != 0 { entity } else { sized_entity(w, path, r.size(), r.align()) };
    // SAFETY: zeroed desc (every callback unset but `serialize`); `serialize` has flecs's `ecs_meta_serialize_t` signature, the
    // serializer pointer erased to `c_void`.
    unsafe {
        let mut d: sys::ecs_opaque_desc_t = core::mem::zeroed();
        d.entity = ent;
        d.type_.as_type = as_type;
        d.type_.serialize = Some(core::mem::transmute::<hyperion_layout::layout::Serialize, unsafe extern "C" fn(*const sys::ecs_serializer_t, *const c_void) -> core::ffi::c_int>(*serialize));
        sys::ecs_opaque_init(w.as_ptr(), &d);
        if let Some(m) = marked {
            sys::ecs_add_id(w.as_ptr(), ent, w.ensure_entity(m));
        }
    }
    ent
}

#[cfg(feature = "meta")]
/// The struct a choice reads as: a member per variant, of its data's type (`bool` for a variant without data), laid out one after
/// another (no value is ever this struct: it only names the variants and their types).
fn choice_type(w: World, path: &str, variants: &'static [hyperion_layout::layout::Variant]) -> Id {
    // SAFETY: an immutable flecs global.
    let boolean = unsafe { sys::FLECS_IDecs_bool_tID_ };
    let mut members = Vec::with_capacity(variants.len());
    let (mut at, mut align) = (0usize, 1usize);
    for v in variants {
        let (ty, size, a) = match v.data {
            Some(d) => (type_of_recipe(w, d), d.size(), d.align()),
            None => (boolean, 1, 1),
        };
        if ty == 0 {
            return 0;
        }
        at = at.next_multiple_of(a.max(1));
        members.push((cs(v.name), ty, at));
        at += size;
        align = align.max(a);
    }
    let ent = sized_entity(w, path, at.next_multiple_of(align).max(1), align);
    // SAFETY: zeroed descs; member names live in `members` for the calls.
    unsafe {
        let ms: Vec<sys::ecs_member_t> = members
            .iter()
            .map(|(name, ty, at)| {
                let mut m: sys::ecs_member_t = core::mem::zeroed();
                m.name = name.as_ptr();
                m.type_ = *ty;
                m.offset = *at as i32;
                m.use_offset = true;
                m
            })
            .collect();
        add_members(w, ent, &ms, false)
    }
}

#[cfg(feature = "meta")]
/// `ecs_struct_init` with `ms`: flecs' desc holds 32 members, the rest are added one by one. 0 on failure.
///
/// # Safety
/// Each member's name lives for the call.
unsafe fn add_members(w: World, entity: Id, ms: &[sys::ecs_member_t], member_entities: bool) -> Id {
    // SAFETY: zeroed desc; the caller's contract for the names.
    unsafe {
        let mut d: sys::ecs_struct_desc_t = core::mem::zeroed();
        d.entity = entity;
        d.create_member_entities = member_entities;
        for (i, m) in ms.iter().take(32).enumerate() {
            d.members[i] = *m;
        }
        if sys::ecs_struct_init(w.as_ptr(), &d) == 0 {
            return 0;
        }
        for m in ms.iter().skip(32) {
            if sys::ecs_struct_add_member(w.as_ptr(), entity, m) != 0 {
                return 0;
            }
        }
    }
    entity
}

#[cfg(feature = "meta")]
/// The serializer of every opaque type (`hyperion_layout::layout::serialize::<T>` hands its value here with `T`'s recipe): each
/// value is handed to flecs's serializer as a flecs type the recipe's form names, which registration made.
///
/// # Safety
/// `ser` is a live `ecs_serializer_t` and `src` a live value of `recipe`'s type.
unsafe fn serialize_recipe(ser: *const c_void, recipe: &'static Recipe, src: *const c_void) -> core::ffi::c_int {
    use hyperion_layout::layout::Form;
    // SAFETY: the caller's contract.
    let ser = unsafe { &*ser.cast::<sys::ecs_serializer_t>() };
    let (Some(value), Recipe::Opaque { form, .. }) = (ser.value, recipe) else { return -1 };
    // SAFETY: the serializer's world is live while it runs; registration made every type the recipe names, so this only looks up.
    let w = unsafe { World::from_raw(ser.world.cast_mut().cast()) };
    let src = src.cast::<u8>();
    // SAFETY (each call below): `value` is flecs's, called with a type and a pointer to a live value of it.
    let words = |s: &str| -> core::ffi::c_int {
        let c = cs(s);
        let p = c.as_ptr();
        unsafe { value(ser, sys::FLECS_IDecs_string_tID_, core::ptr::from_ref(&p).cast()) }
    };
    let mut words = words;
    match form {
        // SAFETY: `src` is a live value of the recipe's type, as each accessor takes.
        Form::Text { get } | Form::Json { get } => unsafe { get(src, &mut words) },
        Form::List { elem, get } => {
            let ty = type_of_recipe(w, elem);
            // SAFETY: as above.
            let (first, n) = unsafe { get(src) };
            if ty == 0 {
                return -1;
            }
            for i in 0..n {
                // SAFETY: element `i` of `n` live ones, `elem.size()` apart.
                if unsafe { value(ser, ty, first.add(i * elem.size()).cast()) } != 0 {
                    return -1;
                }
            }
            0
        }
        Form::Maybe { elem, get } => {
            // SAFETY: as above.
            let p = unsafe { get(src) };
            let ty = type_of_recipe(w, elem);
            if ty == 0 {
                return -1;
            }
            // SAFETY: a non-null `p` is the live value it holds.
            if p.is_null() { 0 } else { unsafe { value(ser, ty, p.cast()) } }
        }
        Form::Ref { elem, get } => {
            // SAFETY: as above.
            let p = unsafe { get(src) }.cast::<c_void>();
            match elem {
                // SAFETY: `p` is a live value of `elem`'s type.
                Recipe::Opaque { .. } => unsafe { serialize_recipe(core::ptr::from_ref(ser).cast(), elem, p) },
                Recipe::Struct { fields, .. } => {
                    let Some(member) = ser.member else { return -1 };
                    for f in fields.iter().filter(|f| !is_hidden(f.ty)) {
                        let (ty, name) = (type_of_recipe(w, f.ty), cs(f.name));
                        // SAFETY: the name lives for the call; the field is a live value of its type, `f.offset` into `p`.
                        if ty == 0 || unsafe { member(ser, name.as_ptr()) } != 0 || unsafe { value(ser, ty, p.cast::<u8>().add(f.offset).cast()) } != 0 {
                            return -1;
                        }
                    }
                    0
                }
                _ => {
                    let ty = type_of_recipe(w, elem);
                    // SAFETY: `p` is a live value of `elem`'s type.
                    if ty == 0 { -1 } else { unsafe { value(ser, ty, p) } }
                }
            }
        }
        Form::OneOf { variants, get } => {
            let Some(member) = ser.member else { return -1 };
            // SAFETY: as above.
            let (i, p) = unsafe { get(src) };
            let Some(v) = variants.get(i) else { return -1 };
            let name = cs(v.name);
            // SAFETY: the name lives for the call.
            if unsafe { member(ser, name.as_ptr()) } != 0 {
                return -1;
            }
            match v.data {
                Some(d) => {
                    let ty = type_of_recipe(w, d);
                    // SAFETY: `p` is the live data of variant `i`.
                    if ty == 0 || p.is_null() { -1 } else { unsafe { value(ser, ty, p.cast()) } }
                }
                // SAFETY: a `bool` on the stack.
                None => unsafe { value(ser, sys::FLECS_IDecs_bool_tID_, core::ptr::from_ref(&true).cast()) },
            }
        }
    }
}

#[cfg(feature = "meta")]
/// A member the console and scripts must not see or write: raw pointers and function pointers (writing one from REST is code
/// execution) and any struct that has nothing else in it. Hidden members keep their bytes and offsets; only the reflection skips them.
fn is_hidden(r: &Recipe) -> bool {
    match r {
        Recipe::Prim { kind: Prim::Ptr, .. } => true,
        // A zero-length array carries only an element type into the hash (`HostVec`'s `data` member): nothing to show.
        Recipe::Array { len: 0, .. } => true,
        Recipe::Array { elem, .. } => is_hidden(elem),
        Recipe::Struct { fields, .. } => fields.iter().all(|f| is_hidden(f.ty)),
        _ => false,
    }
}

#[cfg(feature = "meta")]
/// The members of `r` reflection leaves out (pointers, zero-length arrays, and anything an opaque type holds that is one), as
/// paths from `at` (`[]` an element, `?` an optional value, `.Variant` a choice's data).
#[must_use]
pub fn hidden_members(r: &Recipe, at: &str) -> Vec<String> {
    use hyperion_layout::layout::Form;
    let mut out = Vec::new();
    match r {
        Recipe::Prim { kind: Prim::Ptr, .. } | Recipe::Array { len: 0, .. } => out.push(at.to_owned()),
        Recipe::Array { elem, .. } => out.extend(hidden_members(elem, at)),
        Recipe::Struct { fields, .. } => {
            for f in *fields {
                out.extend(hidden_members(f.ty, &if at.is_empty() { f.name.to_owned() } else { format!("{at}.{}", f.name) }));
            }
        }
        Recipe::Opaque { form: Form::List { elem, .. }, .. } => out.extend(hidden_members(elem, &format!("{at}[]"))),
        Recipe::Opaque { form: Form::Maybe { elem, .. }, .. } => out.extend(hidden_members(elem, &format!("{at}?"))),
        Recipe::Opaque { form: Form::Ref { elem, .. }, .. } => out.extend(hidden_members(elem, at)),
        Recipe::Opaque { form: Form::OneOf { variants, .. }, .. } => {
            for v in *variants {
                if let Some(d) = v.data {
                    out.extend(hidden_members(d, &format!("{at}.{}", v.name)));
                }
            }
        }
        Recipe::Prim { .. } | Recipe::Enum { .. } | Recipe::Opaque { .. } => {}
    }
    out
}

/// What a component holds that reflection leaves out: `mc.Unreflected { members }`, the paths of [`hidden_members`] joined with
/// `, `, on the component's entity. A tool or a test reads it to find a component whose generated type is incomplete.
pub const UNREFLECTED_NAME: &str = "mc.Unreflected";

#[cfg(feature = "meta")]
fn mark_unreflected(w: World, id: Id, members: &[String]) {
    let mut c = w.lookup(UNREFLECTED_NAME);
    // SAFETY: zeroed descs; the member name outlives the call; flecs gives a struct of strings its own hooks (strdup, free).
    unsafe {
        if c == 0 {
            let mut d: sys::ecs_component_desc_t = core::mem::zeroed();
            d.entity = w.ensure_entity(UNREFLECTED_NAME);
            d.type_.size = size_of::<*mut core::ffi::c_char>() as i32;
            d.type_.alignment = align_of::<*mut core::ffi::c_char>() as i32;
            c = sys::ecs_component_init(w.as_ptr(), &d);
            let name = cs("members");
            let mut m: sys::ecs_member_t = core::mem::zeroed();
            m.name = name.as_ptr();
            m.type_ = sys::FLECS_IDecs_string_tID_;
            add_members(w, c, &[m], false);
        }
        let text = cs(&members.join(", "));
        let slot = sys::ecs_ensure_id(w.as_ptr(), id, c, size_of::<*mut core::ffi::c_char>()).cast::<*mut core::ffi::c_char>();
        if slot.is_null() {
            return;
        }
        if let Some(free) = sys::ecs_os_api.free_ {
            if !(*slot).is_null() {
                free((*slot).cast());
            }
        }
        *slot = sys::ecs_os_api.strdup_.map_or(core::ptr::null_mut(), |dup| dup(text.as_ptr()));
        sys::ecs_modified_id(w.as_ptr(), id, c);
    }
}

#[cfg(feature = "meta")]
/// `ecs_struct` reflection from the recipe (members, offsets). Pointer members are skipped (hidden). Returns 0 when nothing is visible.
fn register_struct(w: World, r: &'static Recipe, path: &str, entity: Id) -> Id {
    let Recipe::Struct { fields, .. } = r else { return 0 };
    let visible: Vec<_> = fields.iter().filter(|f| !is_hidden(f.ty)).collect();
    if visible.is_empty() {
        return 0;
    }
    let names: Vec<_> = visible.iter().map(|f| cs(f.name)).collect();
    let mut ms: Vec<sys::ecs_member_t> = Vec::with_capacity(visible.len());
    for (i, f) in visible.iter().enumerate() {
        let Some(mt) = member_type(w, path, f.name, f.ty) else {
            hyperion_log(&format!("component {path} field '{}': not reflectable, member skipped", f.name));
            continue;
        };
        // SAFETY: a zeroed member is flecs's "unset".
        let mut m: sys::ecs_member_t = unsafe { core::mem::zeroed() };
        m.name = names[i].as_ptr();
        m.type_ = mt.ty;
        m.count = mt.count;
        m.offset = f.offset as i32;
        m.use_offset = true;
        ms.push(m);
    }
    if ms.is_empty() {
        return 0;
    }
    // flecs creates `<component>.<member>` entities only on request. The settings pump (`assets/ecs/server/pump.flecs`, `mc.Pump.member`) names a
    // `server.*` singleton's member by that path, so those get them; every other component stays a plain struct.
    // SAFETY: member names live in `names` for the call.
    unsafe { add_members(w, entity, &ms, path.starts_with("server.")) }
}

/// Hash recorded on the world's component `name`, or None when it is not registered.
pub fn world_layout_hash(w: World, name: &str) -> Option<u64> {
    let e = w.lookup(name);
    if e == 0 {
        return None;
    }
    w.get_by_id::<u64>(e, layout_hash_component(w))
}

/// True when `e` is a bare named entity a script created before its provider registered the component (`drops.Broken { journal.Journaled }` in a
/// settings script): no type info and no recorded layout hash. Registration adopts it (flecs reuses the entity by name, so its tags survive) instead of
/// refusing it as a layout mismatch.
fn is_placeholder(w: World, e: Id) -> bool {
    // SAFETY: live world; `e` is a live entity.
    let typed = unsafe { !sys::ecs_get_type_info(w.as_ptr(), e).is_null() };
    !typed && w.get_by_id::<u64>(e, layout_hash_component(w)).is_none()
}

fn set_hash(w: World, id: Id, h: u64) {
    w.set_by_id(id, layout_hash_component(w), &h);
}

/// PROVIDER side: register `T` under `name` (data layout, reflection) and record its layout hash. Idempotent when the layout is
/// unchanged. A different layout is an error: the old component must be removed first.
pub fn register_component<T: Layout + Copy>(w: World, name: &str) -> Result<Id, Error> {
    register::<T>(w, name, None)
}

/// [`register_component`] for a host-owned component that holds heap data: `hooks` (ctor, dtor, copy, move) are functions of the
/// host image, which outlives every world, so they may be registered where a module's may not. Modules only [`require_component`].
pub fn register_component_with_hooks<T: Layout>(w: World, name: &str, hooks: &sys::ecs_type_hooks_t) -> Result<Id, Error> {
    register::<T>(w, name, Some(hooks))
}

fn register<T: Layout>(w: World, name: &str, hooks: Option<&sys::ecs_type_hooks_t>) -> Result<Id, Error> {
    let want = T::HASH;
    let existing = w.lookup(name);
    if existing != 0 && !is_placeholder(w, existing) {
        let have = w.get_by_id::<u64>(existing, layout_hash_component(w));
        if have == Some(want) {
            return Ok(existing);
        }
        return Err(Error::LayoutChanged { name: name.into(), world: have, built_against: want });
    }
    #[cfg(feature = "meta")]
    let claimed = if existing == 0 { claim_shape::<T>(w, name) } else { 0 };
    #[cfg(not(feature = "meta"))]
    let claimed = 0;
    if claimed != 0
        && let Some(h) = hooks
    {
        // SAFETY: live world; the claimed type was only ever a member's type, so no entity holds it yet and flecs takes its
        // hooks as a new component's (`ecs_component_init` would not: it returns early for an existing component).
        unsafe { sys::ecs_set_hooks_id(w.as_ptr(), claimed, h) };
    }
    let id = if claimed != 0 { claimed } else { init_component(w, name, size_of::<T>(), align_of::<T>(), hooks) };
    if id == 0 {
        return Err(Error::RegisterFailed(name.into()));
    }
    #[cfg(feature = "meta")]
    match T::RECIPE {
        _ if claimed != 0 => {}
        Recipe::Struct { fields, .. } => {
            // 0 is also "every member hidden" (a pure pointer table): not an error.
            if !fields.is_empty() && register_struct(w, T::RECIPE, name, id) == 0 && !fields.iter().all(|f| is_hidden(f.ty)) {
                return Err(Error::RegisterFailed(name.into()));
            }
        }
        Recipe::Enum { kind: EnumKind::Enum, consts, tag, .. } => {
            if enum_type(w, name, id, consts, tag).is_none() {
                return Err(Error::RegisterFailed(name.into()));
            }
        }
        Recipe::Opaque { .. } => {
            if opaque_type(w, name, id, T::RECIPE) == 0 {
                return Err(Error::RegisterFailed(name.into()));
            }
        }
        _ => {}
    }
    #[cfg(feature = "meta")]
    {
        if matches!(T::RECIPE, Recipe::Struct { .. } | Recipe::Enum { kind: EnumKind::Enum, .. } | Recipe::Opaque { .. }) && size_of::<T>() != 0 {
            adopt_shape(w, T::RECIPE, id);
        }
        let hidden = hidden_members(T::RECIPE, "");
        if !hidden.is_empty() && size_of::<T>() != 0 {
            mark_unreflected(w, id, &hidden);
        }
    }
    if hooks.is_none() && !hook_free(w, id) {
        return Err(Error::RegisterFailed(format!("{name}: flecs type hooks (ctor, dtor, copy, move) are not allowed; components are Copy plain data")));
    }
    set_hash(w, id, want);
    Ok(id)
}

/// Register `name` with an explicit `recipe` and layout `hash` (a table whose recorded hash is not the recipe hash: a service header, a
/// raw ABI vtable). Members that are pointers are hidden, so the explorer shows the scalar header fields and never a function pointer.
#[cfg(feature = "meta")]
pub fn register_reflected(w: World, name: &str, size: usize, align: usize, hash: u64, recipe: &'static Recipe) -> Result<Id, Error> {
    if w.lookup(name) != 0 && !is_placeholder(w, w.lookup(name)) {
        return Err(Error::LayoutChanged { name: name.into(), world: world_layout_hash(w, name), built_against: hash });
    }
    let id = init_component(w, name, size, align, None);
    if id == 0 {
        return Err(Error::RegisterFailed(name.into()));
    }
    if let Recipe::Struct { fields, .. } = recipe {
        if !fields.is_empty() {
            // A table with only pointers has nothing to show: it stays unreflected on purpose (tools/reflection-opaque.txt).
            register_struct(w, recipe, name, id);
        }
    }
    if !hook_free(w, id) {
        return Err(Error::RegisterFailed(format!("{name}: flecs type hooks (ctor, dtor, copy, move) are not allowed; components are Copy plain data")));
    }
    set_hash(w, id, hash);
    Ok(id)
}

/// A table holding pointers (`mc.Host`, `mc.Game`): reflected like any component, but its pointer members are hidden, so REST and
/// scripts can read its version and padding fields and can never read or overwrite a function pointer.
#[cfg(feature = "meta")]
pub fn register_opaque<T: Layout>(w: World, name: &str) -> Result<Id, Error> {
    register_reflected(w, name, size_of::<T>(), align_of::<T>(), T::HASH, T::RECIPE)
}

/// Register an opaque component by raw `size`, `align` and layout `hash` (no Rust type, no reflection): the host stands in for a provider
/// whose type it does not link, e.g. a stub for `mc.Host` when loading a Zig-built module in a test. Production code uses
/// [`register_component`].
pub fn register_raw(w: World, name: &str, size: usize, align: usize, hash: u64) -> Result<Id, Error> {
    if w.lookup(name) != 0 && !is_placeholder(w, w.lookup(name)) {
        return Err(Error::LayoutChanged { name: name.into(), world: world_layout_hash(w, name), built_against: hash });
    }
    let id = init_component(w, name, size, align, None);
    if id == 0 {
        return Err(Error::RegisterFailed(name.into()));
    }
    if !hook_free(w, id) {
        return Err(Error::RegisterFailed(format!("{name}: flecs type hooks (ctor, dtor, copy, move) are not allowed; components are Copy plain data")));
    }
    set_hash(w, id, hash);
    Ok(id)
}

/// CONSUMER side: find `name` and verify the world's hash equals the one this module was compiled against.
pub fn require_component<T: Layout>(w: World, name: &str) -> Result<Id, Error> {
    let e = w.lookup(name);
    if e == 0 {
        return Err(Error::NotAComponent(name.into()));
    }
    let have = w.get_by_id::<u64>(e, layout_hash_component(w));
    if have != Some(T::HASH) {
        return Err(Error::LayoutChanged { name: name.into(), world: have, built_against: T::HASH });
    }
    Ok(e)
}

/// Silent variant: 0 when missing or mismatched.
pub fn require_component_quiet<T: Layout>(w: World, name: &str) -> Id {
    require_component::<T>(w, name).unwrap_or(0)
}

/// Remove a component everywhere (plain and as relationship) and delete its entity (migration, rollback).
pub fn drop_component(w: World, id: Id) {
    // SAFETY: world live; called between ticks with every user of the component deactivated.
    unsafe {
        sys::ecs_remove_all(w.as_ptr(), id);
        sys::ecs_remove_all(w.as_ptr(), crate::world::pair(id, sys::EcsWildcard));
        sys::ecs_delete(w.as_ptr(), id);
    }
}
