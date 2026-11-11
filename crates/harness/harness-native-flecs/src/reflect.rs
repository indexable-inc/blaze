//! The world described from flecs's own reflection: what each component holds, which the host keeps, what reflection still
//! leaves out, and (with the json addon) the rows a query string matches.
//!
//! This is the schema the repl turns into types for a cell's check (docs/repl-live-types.md): it is read from the world as it
//! is now, so a component a plugin or a script registered later is in it, and nothing is written by hand on either side.
use crate::WorldRef;
use core::ffi::{CStr, c_char};
use hyperion_ecs::component::{UNREFLECTED_NAME, mark};
use hyperion_ecs::sys;
#[cfg(feature = "json")]
use std::ffi::CString;

/// The tag on a component's entity that says den-native keeps it: it writes the component itself (every frame, or from the
/// daemon's state), so the schema gives cells a read-only type for it and a cell's write to it is refused. A component is the
/// host's from its registration until [`ComponentView::writable`](crate::ComponentView::writable) says scene scripts and cells
/// may write it.
pub const HOST_OWNED: &str = "den.HostOwned";

/// Takes a string flecs allocated: a copy, and the original freed on flecs's allocator.
///
/// # Safety
/// `p` is null or a NUL-terminated string from flecs's allocator that nothing else frees.
unsafe fn take(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller's contract.
    let out = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    // SAFETY: `free_` is flecs's allocator, read by value; `p` came from it and is freed once, here.
    if let Some(free) = unsafe { sys::ecs_os_api.free_ } {
        unsafe { free(p.cast()) };
    }
    Some(out)
}

/// Root scopes whose components are not the world's own: flecs's (its builtins print without the `flecs.core.` prefix, so a
/// path cannot tell), hyperion's shared types (`types.r<key>`) and its layout hash (`mc.LayoutHash`).
const HIDDEN_SCOPES: [&str; 3] = ["flecs", "types", "mc"];

/// Whether `e` lives under one of [`HIDDEN_SCOPES`].
///
/// # Safety
/// `w` is a live world and `e` an entity alive in it.
unsafe fn hidden(w: *mut sys::ecs_world_t, e: sys::ecs_entity_t) -> bool {
    let mut root = e;
    // SAFETY: the caller's contract; parents of a live entity are alive.
    unsafe {
        loop {
            let parent = sys::ecs_get_parent(w, root);
            if parent == 0 {
                break;
            }
            root = parent;
        }
        if root == e {
            return false;
        }
        let name = sys::ecs_get_name(w, root);
        !name.is_null() && CStr::from_ptr(name).to_str().is_ok_and(|n| HIDDEN_SCOPES.contains(&n))
    }
}

/// `text` as a JSON string.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Component `C` of entity `e`, if it has one.
///
/// # Safety
/// `w` is live, and `C` is the type of component `id`.
unsafe fn get<'w, C>(w: *mut sys::ecs_world_t, e: sys::ecs_entity_t, id: sys::ecs_id_t) -> Option<&'w C> {
    // SAFETY: the caller's contract; flecs returns null or a pointer to a live `C` of `e`.
    unsafe { sys::ecs_get_id(w, e, id).cast::<C>().as_ref() }
}

/// The entity at `path`, 0 when there is none.
///
/// # Safety
/// `w` is live.
unsafe fn lookup(w: *mut sys::ecs_world_t, path: &CStr) -> sys::ecs_entity_t {
    // SAFETY: the caller's contract; the path is NUL-terminated.
    unsafe { sys::ecs_lookup(w, path.as_ptr()) }
}

/// Reads type entities as den's schema: flecs's type info JSON (`["float"]`, `{"x": ...}`, `["enum", ...]`, `["array", T, n]`,
/// `["vector", T]`), the same text `ecs_type_info_to_json` writes but compact, plus what den's marks on an opaque type say and
/// flecs's type info cannot: `["option", T]`, `["oneof", {"Variant": T}]` and `["json"]`. A value inside a component whose
/// type is another component's (every colour member is `Rgba`: hyperion-ecs shares one type per shape) is `["ref", "Rgba"]`,
/// where flecs's own type info would repeat the struct, so the repl gives every colour one type.
struct Walk {
    w: *mut sys::ecs_world_t,
    optional: sys::ecs_entity_t,
    choice: sys::ecs_entity_t,
    json: sys::ecs_entity_t,
    /// Where reflection could not say what a type holds, as paths from the component.
    gaps: Vec<String>,
}

impl Walk {
    /// # Safety
    /// `w` is a live world.
    unsafe fn new(w: *mut sys::ecs_world_t) -> Self {
        // SAFETY: the caller's contract.
        unsafe {
            Self {
                w,
                optional: lookup(w, &cstr(mark::OPTIONAL)),
                choice: lookup(w, &cstr(mark::CHOICE)),
                json: lookup(w, &cstr(mark::JSON)),
                gaps: Vec::new(),
            }
        }
    }

    fn has(&self, e: sys::ecs_entity_t, id: sys::ecs_id_t) -> bool {
        // SAFETY: `self.w` is live; `e` is a type entity of it.
        id != 0 && unsafe { sys::ecs_has_id(self.w, e, id) }
    }

    /// The type `t`, read at `at` (for a gap's path; empty for the component itself).
    fn ty(&mut self, t: sys::ecs_entity_t, at: &str) -> String {
        let w = self.w;
        // SAFETY: `w` is live and `t` one of its entities; each component is read as its own type.
        unsafe {
            if !at.is_empty() && self.has(t, sys::FLECS_IDEcsStructID_) && !hidden(w, t) {
                if let Some(path) = take(sys::ecs_get_path_w_sep(w, 0, t, c".".as_ptr(), core::ptr::null())) {
                    return format!("[\"ref\",{}]", quote(&path));
                }
            }
            if let Some(p) = get::<sys::EcsPrimitive>(w, t, sys::FLECS_IDEcsPrimitiveID_) {
                return format!("[{}]", quote(primitive(p.kind)));
            }
            if self.has(t, sys::FLECS_IDEcsEnumID_) {
                return format!("[\"enum\"{}]", self.constants(t));
            }
            if self.has(t, sys::FLECS_IDEcsBitmaskID_) {
                return format!("[\"bitmask\"{}]", self.constants(t));
            }
            if let Some(o) = get::<sys::EcsOpaque>(w, t, sys::FLECS_IDEcsOpaqueID_) {
                let o = *o;
                if o.serialize.is_none() {
                    self.gaps.push(format!("{at} (an opaque type with no serializer)"));
                    return "\"opaque\"".to_owned();
                }
                if self.has(t, self.json) {
                    return "[\"json\"]".to_owned();
                }
                if self.has(t, self.optional) {
                    let Some(v) = get::<sys::EcsVector>(w, o.as_type, sys::FLECS_IDEcsVectorID_) else {
                        self.gaps.push(format!("{at} (an optional value that is not a vector)"));
                        return "\"opaque\"".to_owned();
                    };
                    return format!("[\"option\",{}]", self.ty(v.type_, &format!("{at}?")));
                }
                if self.has(t, self.choice) {
                    return format!("[\"oneof\",{}]", self.ty(o.as_type, at));
                }
                return self.ty(o.as_type, at);
            }
            if let Some(s) = get::<sys::EcsStruct>(w, t, sys::FLECS_IDEcsStructID_) {
                let members = core::slice::from_raw_parts(s.members.array.cast::<sys::ecs_member_t>(), usize::try_from(s.members.count).unwrap_or(0));
                let mut out = Vec::with_capacity(members.len());
                for m in members {
                    let name = if m.name.is_null() { String::new() } else { CStr::from_ptr(m.name).to_string_lossy().into_owned() };
                    let here = if at.is_empty() { name.clone() } else { format!("{at}.{name}") };
                    let inner = self.ty(m.type_, &here);
                    let ty = if m.count > 0 { format!("[\"array\",{inner},{}]", m.count) } else { inner };
                    out.push(format!("{}:{ty}", quote(&name)));
                }
                return format!("{{{}}}", out.join(","));
            }
            if let Some(a) = get::<sys::EcsArray>(w, t, sys::FLECS_IDEcsArrayID_) {
                let (elem, count) = (a.type_, a.count);
                return format!("[\"array\",{},{count}]", self.ty(elem, &format!("{at}[]")));
            }
            if let Some(v) = get::<sys::EcsVector>(w, t, sys::FLECS_IDEcsVectorID_) {
                let elem = v.type_;
                return format!("[\"vector\",{}]", self.ty(elem, &format!("{at}[]")));
            }
        }
        self.gaps.push(if at.is_empty() { "(the component itself)".to_owned() } else { at.to_owned() });
        "\"opaque\"".to_owned()
    }

    /// An enum's or bitmask's constants as `,"A","B"`, in the order flecs keeps them (its children).
    fn constants(&self, t: sys::ecs_entity_t) -> String {
        let mut out = String::new();
        // SAFETY: `self.w` is live; the iterator runs to its end, which finishes it.
        unsafe {
            let mut it = sys::ecs_each_id(self.w, hyperion_ecs::pair(sys::EcsChildOf, t));
            while sys::ecs_each_next(&mut it) {
                for i in 0..usize::try_from(it.count).unwrap_or(0) {
                    let name = sys::ecs_get_name(self.w, *it.entities.add(i));
                    if !name.is_null() {
                        out.push(',');
                        out.push_str(&quote(&CStr::from_ptr(name).to_string_lossy()));
                    }
                }
            }
        }
        out
    }
}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap_or_default()
}

/// flecs's type info name of a primitive kind (as `flecs_json_typeinfo_ser_primitive` writes it).
fn primitive(kind: sys::ecs_primitive_kind_t) -> &'static str {
    match kind {
        sys::EcsBool => "bool",
        sys::EcsChar | sys::EcsString => "text",
        sys::EcsByte => "byte",
        sys::EcsF32 | sys::EcsF64 => "float",
        sys::EcsEntity => "entity",
        sys::EcsId => "id",
        _ => "int",
    }
}

/// One component of the world: its path, its id and its size.
struct Found {
    path: String,
    id: sys::ecs_entity_t,
    size: i32,
}

impl WorldRef<'_> {
    /// Every component of the world but flecs's and hyperion's own, by path (`.` between parents), sorted.
    fn components(self) -> Vec<Found> {
        let w = self.raw().as_ptr();
        let mut found = Vec::new();
        // SAFETY: `w` is a live world; the iterator is run to its end, which finishes it; each path is taken (freed) once.
        unsafe {
            let mut it = sys::ecs_each_id(w, sys::FLECS_IDEcsComponentID_);
            while sys::ecs_each_next(&mut it) {
                for i in 0..usize::try_from(it.count).unwrap_or(0) {
                    let e = *it.entities.add(i);
                    if hidden(w, e) {
                        continue;
                    }
                    let Some(path) = take(sys::ecs_get_path_w_sep(w, 0, e, c".".as_ptr(), core::ptr::null())) else { continue };
                    let size = get::<sys::EcsComponent>(w, e, sys::FLECS_IDEcsComponentID_).map_or(0, |c| c.size);
                    found.push(Found { path, id: e, size });
                }
            }
        }
        found.sort_by(|a, b| a.path.cmp(&b.path));
        found
    }

    /// The world's components as den's schema: `{"components":{"Pos":{"x":["float"],"y":["float"]},"Mode":null},"host":["Rect"]}`.
    /// A component's value is its type as [`Walk`] reads it (`null` for a tag, `"opaque"` for data reflection cannot describe);
    /// `host` names the components den-native keeps ([`HOST_OWNED`] on their entity).
    #[must_use]
    pub fn schema_json(self) -> String {
        let w = self.raw().as_ptr();
        // SAFETY: `w` is a live world.
        let (mut walk, host) = unsafe { (Walk::new(w), lookup(w, &cstr(HOST_OWNED))) };
        let mut members = Vec::new();
        let mut kept = Vec::new();
        for c in self.components() {
            let ty = if c.size == 0 { "null".to_owned() } else { walk.ty(c.id, "") };
            members.push(format!("{}:{ty}", quote(&c.path)));
            if walk.has(c.id, host) {
                kept.push(quote(&c.path));
            }
        }
        format!("{{\"components\":{{{}}},\"host\":[{}]}}", members.join(","), kept.join(","))
    }

    /// Whether component `name` is den-native's own ([`HOST_OWNED`]): what a cell's write to it is refused by.
    #[must_use]
    pub fn host_owned(self, name: &str) -> bool {
        let w = self.raw().as_ptr();
        // SAFETY: `w` is a live world; both paths are NUL-terminated.
        unsafe {
            let (c, host) = (lookup(w, &cstr(name)), lookup(w, &cstr(HOST_OWNED)));
            c != 0 && host != 0 && sys::ecs_has_id(w, c, host)
        }
    }

    /// Every component whose type the schema cannot give in full, with what is missing: data reflection cannot describe, an
    /// opaque type without a serializer, or members registration left out (`mc.Unreflected`). Empty when every generated type is
    /// complete.
    #[must_use]
    pub fn reflection_gaps(self) -> Vec<String> {
        let w = self.raw().as_ptr();
        // SAFETY: `w` is a live world.
        let (mut walk, unreflected) = unsafe { (Walk::new(w), lookup(w, &cstr(UNREFLECTED_NAME))) };
        let mut gaps = Vec::new();
        for c in self.components() {
            if c.size == 0 {
                continue;
            }
            walk.gaps.clear();
            walk.ty(c.id, "");
            gaps.extend(walk.gaps.drain(..).map(|g| format!("{}: {g}", c.path)));
            // SAFETY: `mc.Unreflected` is `{ members: string }`, a `char*` on flecs's allocator.
            if let Some(m) = (unreflected != 0).then(|| unsafe { get::<*const c_char>(w, c.id, unreflected) }).flatten() {
                // SAFETY: a non-null string flecs keeps for the component.
                let text = if m.is_null() { String::new() } else { unsafe { CStr::from_ptr(*m) }.to_string_lossy().into_owned() };
                gaps.push(format!("{}: hidden members {text}", c.path));
            }
        }
        gaps
    }

    /// The entities `expr` (flecs's query language: `Pos, ?Vel, !Frozen`) matches, as flecs's `ecs_iter_to_json` writes them
    /// with values: `{"results":[{"name":"ship","fields":{"values":[{"x":1,"y":2},0]}}]}`. A query flecs cannot read is an error
    /// naming it; flecs logs the reason.
    #[cfg(feature = "json")]
    pub fn query_json(self, expr: &str) -> Result<String, String> {
        let text = CString::new(expr).map_err(|_| format!("a query cannot hold a NUL: {expr:?}"))?;
        let w = self.raw().as_ptr();
        // SAFETY: zeroed descs are flecs's "unset"; `text` outlives the init (flecs parses it there); the iterator is run to
        // its end by the serializer, the query is finished after, and the string is taken once.
        unsafe {
            let mut d: sys::ecs_query_desc_t = core::mem::zeroed();
            d.expr = text.as_ptr();
            let q = sys::ecs_query_init(w, &d);
            if q.is_null() {
                return Err(format!("flecs could not read the query `{expr}`"));
            }
            let mut it = sys::ecs_query_iter(w, q);
            let mut desc: sys::ecs_iter_to_json_desc_t = core::mem::zeroed();
            desc.serialize_values = true;
            desc.serialize_fields = true;
            let out = take(sys::ecs_iter_to_json(&mut it, &desc));
            sys::ecs_query_fini(q);
            out.ok_or_else(|| format!("flecs could not serialize the rows of `{expr}`"))
        }
    }

    /// Entity `path` (`hn.Theme.card`) as flecs's `ecs_entity_to_json` writes it: its parent and name, its tags and pairs,
    /// and every component with its value, those it inherits from a prefab too. An error names a path nothing has.
    #[cfg(feature = "json")]
    pub fn entity_json(self, path: &str) -> Result<String, String> {
        let w = self.raw().as_ptr();
        // SAFETY: a live world; a zeroed desc is flecs's "unset"; the string is taken once.
        unsafe {
            let e = lookup(w, &cstr(path));
            if e == 0 {
                return Err(format!("the world has no entity `{path}`"));
            }
            let mut d: sys::ecs_entity_to_json_desc_t = core::mem::zeroed();
            d.serialize_values = true;
            d.serialize_full_paths = true;
            d.serialize_inherited = true;
            take(sys::ecs_entity_to_json(w, e, &d)).ok_or_else(|| format!("flecs could not serialize `{path}`"))
        }
    }

    /// Write `json` into component `component` of entity `path` and tell what observes it: the members `json` names
    /// change, the rest stay (a component the entity only inherited becomes its own first, with the inherited value). The
    /// component's value after, as JSON.
    ///
    /// # Errors
    /// Naming what refused: no such entity or component, a component with no data, one the host keeps
    /// ([`HOST_OWNED`]: it writes it itself, so a write would be lost), or a value that does not fit its type.
    #[cfg(feature = "json")]
    pub fn set_json(self, path: &str, component: &str, json: &str) -> Result<String, String> {
        let w = self.raw().as_ptr();
        let text = CString::new(json).map_err(|_| format!("a value cannot hold a NUL: {json:?}"))?;
        if self.host_owned(component) {
            return Err(format!("`{component}` is the app's own ({HOST_OWNED}): it writes it itself, so a write to `{path}` would be lost"));
        }
        // SAFETY: a live world, outside a frame (not deferred); `ensure` gives the entity's own value of the component's
        // size, live until the next structural change, which nothing here makes before it is read back.
        unsafe {
            let e = lookup(w, &cstr(path));
            if e == 0 {
                return Err(format!("the world has no entity `{path}`"));
            }
            let c = lookup(w, &cstr(component));
            if c == 0 {
                return Err(format!("the world has no component `{component}`"));
            }
            let info = sys::ecs_get_type_info(w, c);
            if info.is_null() || (*info).size <= 0 {
                return Err(format!("`{component}` holds no data (a tag): there is nothing to set on `{path}`"));
            }
            let p = sys::ecs_ensure_id(w, e, c, (*info).size as usize);
            if p.is_null() || sys::ecs_ptr_from_json(w, c, p, text.as_ptr(), core::ptr::null()).is_null() {
                return Err(format!("{json} does not fit `{component}` (its type: world.schema(\"{component}\"))"));
            }
            sys::ecs_modified_id(w, e, c);
            take(sys::ecs_ptr_to_json(w, c, p)).ok_or_else(|| format!("flecs could not serialize `{component}` of `{path}`"))
        }
    }

    /// flecs's own `ecs_type_info_to_json` of component `name` (the json addon), which [`schema_json`](Self::schema_json)'s text
    /// for a type without den's marks equals but for spacing.
    #[cfg(feature = "json")]
    #[must_use]
    pub fn type_info_json(self, name: &str) -> Option<String> {
        let w = self.raw().as_ptr();
        // SAFETY: `w` is live; the path is NUL-terminated; the string is taken once.
        unsafe {
            let c = lookup(w, &cstr(name));
            if c == 0 || !sys::ecs_has_id(w, c, sys::FLECS_IDEcsTypeSerializerID_) {
                return None;
            }
            take(sys::ecs_type_info_to_json(w, c))
        }
    }
}
