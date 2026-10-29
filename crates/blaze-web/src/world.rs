//! The functions of each workspace's program as a Flecs world, as its builds' records say them,
//! the snapshot of it a page is sent first, and Flecs's REST over it (docs/web.md).
//!
//! What it holds:
//!
//! - a workspace: [`Workspace`], unnamed; in it (Flecs's `Parent`) each crate the build put a
//!   function of, `Crate`, named as the crate, and in a crate its modules, `Module`, named as
//!   the module, one in another as in the crate;
//! - a function: [`Function`], unnamed, in the module its path is in ([`module_path`]: a
//!   type's path is a module here, as on the page), or the crate when it is in none, and
//!   `Callees`, the functions it calls;
//! - a module's `Callees`, the other modules its functions call, and `(Calls, crate)` on a
//!   crate one of whose functions calls one of that crate's. A call that is gone stays until
//!   the program is written again. Only crates' calls are pairs: anim's 150 000 calls of
//!   functions would hold 266 MB as pairs that fragment and 1.9 GB as pairs that do not, and
//!   with its 20 000 calls of modules as pairs and `ChildOf` for `Parent`, the daemon held
//!   318 MB after anim's build and two edits, against 163 MB.
//!
//! One thread has the world: [`World`] is not `Send`.

use std::{
    ffi::{CStr, CString, c_char, c_void},
    path::{Path, PathBuf},
};

use blaze_protocol::events::{Event, How, Record};
use flecs_c as ecs;
use rustc_hash::FxHashMap;

type Entity = ecs::ecs_entity_t;

/// The kinds the world's entities have, as Flecs script.
const KINDS: &str = "
struct Workspace(root: string, build: u64)
struct Function(key: u64, name: string, how: u8, ns: u64, build: u64)
vector Callees(entity)
Crate {}
Module {}
Calls { Relationship }
";

/// A workspace, as Flecs has it (its layout is [`KINDS`]'s).
#[repr(C)]
struct Workspace {
    root: *const c_char,
    build: u64,
}

/// A function, as Flecs has it (its layout is [`KINDS`]'s). `how` is the snapshot's number
/// for it (docs/web.md).
#[repr(C)]
struct Function {
    key: u64,
    name: *const c_char,
    how: u8,
    ns: u64,
    build: u64,
}

/// Flecs's `EcsParent`.
#[repr(C)]
struct Parent {
    value: Entity,
}

/// What a record did to a program.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// It is in the program as the page is told it: the page is sent it.
    Told,
    /// It began the program again: the page is sent it anew.
    Anew,
}

/// A Flecs world of programs.
pub struct World {
    world: *mut ecs::ecs_world_t,
    rest: *mut ecs::ecs_http_server_t,
    workspace: Entity,
    function: Entity,
    callees: Entity,
    krate: Entity,
    module: Entity,
    calls: Entity,
}

/// The program a workspace builds, and the records of its last build.
pub struct Program {
    root: PathBuf,
    entity: Entity,
    build: u64,
    crates: Vec<String>,
    /// Each crate's entity, by its index in `crates`.
    crates_of: Vec<Entity>,
    crate_of: FxHashMap<String, (u32, Entity)>,
    modules: FxHashMap<(Entity, String), Entity>,
    /// Each function, in the snapshot's order.
    nodes: Vec<Node>,
    by_key: FxHashMap<u64, u32>,
    /// The nodes of each object of the build under way, by their ids in it.
    ids: FxHashMap<u32, FxHashMap<u32, u32>>,
    /// The nodes each node calls.
    out: Vec<Vec<u32>>,
    edges: usize,
    /// The modules that call one another, and the crates.
    calls: rustc_hash::FxHashSet<(Entity, Entity)>,
    /// The modules each module calls, as its `Callees`.
    module_calls: FxHashMap<Entity, Vec<Entity>>,
    /// Calls of the build's objects to ids none of its records has named yet: the object,
    /// the node that calls and the id. One to code of another is never named.
    unresolved: Vec<(u32, u32, u32)>,
    /// The records of the last build, but for its functions.
    records: Vec<Record>,
}

struct Node {
    entity: Entity,
    krate: u32,
    /// The module it is in, or its crate.
    module: Entity,
}

/// What Flecs's REST answered.
pub struct Answer {
    pub code: u16,
    pub kind: String,
    pub body: Vec<u8>,
}

impl World {
    pub fn new() -> Result<World, String> {
        // SAFETY: a world of its own, and NUL-terminated strings.
        unsafe {
            let world = ecs::ecs_init();
            if world.is_null() {
                return Err("Flecs made no world".into());
            }
            let code = CString::new(KINDS).map_err(|e| e.to_string())?;
            if ecs::ecs_script_run(
                world,
                c"blaze-web".as_ptr(),
                code.as_ptr(),
                std::ptr::null_mut(),
            ) != 0
            {
                ecs::ecs_fini(world);
                return Err("Flecs did not take the build page's kinds".into());
            }
            let rest = ecs::glue_rest_new(world);
            if rest.is_null() {
                ecs::ecs_fini(world);
                return Err("Flecs made no REST server".into());
            }
            let named = |name: &CStr| ecs::ecs_lookup(world, name.as_ptr());
            Ok(World {
                world,
                rest,
                workspace: named(c"Workspace"),
                function: named(c"Function"),
                callees: named(c"Callees"),
                krate: named(c"Crate"),
                module: named(c"Module"),
                calls: named(c"Calls"),
            })
        }
    }

    /// A program of the workspace at `root`, with no functions yet.
    pub fn program(&mut self, root: &Path) -> Program {
        let entity = self.workspace_of(root, 0);
        Program {
            root: root.to_owned(),
            entity,
            build: 0,
            crates: Vec::new(),
            crates_of: Vec::new(),
            crate_of: FxHashMap::default(),
            modules: FxHashMap::default(),
            nodes: Vec::new(),
            by_key: FxHashMap::default(),
            ids: FxHashMap::default(),
            out: Vec::new(),
            edges: 0,
            calls: Default::default(),
            module_calls: FxHashMap::default(),
            unresolved: Vec::new(),
            records: Vec::new(),
        }
    }

    /// Takes the program out of the world.
    pub fn forget(&mut self, program: Program) {
        // SAFETY: the world's thread; children go with the workspace.
        unsafe { ecs::ecs_delete(self.world, program.entity) };
    }

    /// Takes in `record`, of build `build` of the program's workspace.
    pub fn apply(&mut self, program: &mut Program, build: u64, record: &Record) -> Applied {
        match &record.event {
            Event::Began { .. } => {
                program.build = build;
                program.ids.clear();
                program.unresolved.clear();
                program.records.clear();
                program.records.push(record.clone());
                self.set_workspace(program);
                Applied::Told
            }
            Event::Functions { object, whole, functions } => {
                let anew = *whole && !program.ids.contains_key(object);
                if anew {
                    self.forget_functions(program);
                }
                self.placed(program, *object, functions);
                if anew { Applied::Anew } else { Applied::Told }
            }
            Event::Finished { .. } => {
                program.unresolved.clear();
                program.records.push(record.clone());
                Applied::Told
            }
            _ => {
                program.records.push(record.clone());
                Applied::Told
            }
        }
    }

    /// What Flecs's REST answers `method` of `path` (with its query), read only.
    pub fn rest(&mut self, method: &str, path: &str) -> Result<Answer, String> {
        let method = CString::new(method).map_err(|_| "a NUL in the method")?;
        let path = CString::new(path).map_err(|_| "a NUL in the path")?;
        let (mut body, mut len, mut kind) = (std::ptr::null_mut(), 0i32, std::ptr::null());
        // SAFETY: the world's thread, its server, NUL-terminated strings; the body is Flecs's
        // to free, once copied.
        unsafe {
            let code = ecs::glue_rest_request(
                self.rest,
                method.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                &mut body,
                &mut len,
                &mut kind,
            );
            let bytes = if body.is_null() || len <= 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(body.cast::<u8>(), len as usize).to_vec()
            };
            ecs::glue_free(body.cast::<c_void>());
            let kind = if kind.is_null() {
                "application/json".to_owned()
            } else {
                CStr::from_ptr(kind).to_string_lossy().into_owned()
            };
            let code = u16::try_from(code).map_err(|_| format!("Flecs answered {code}"))?;
            Ok(Answer { code, kind, body: bytes })
        }
    }

    /// The program in the layout of docs/web.md, "The snapshot".
    pub fn snapshot(&self, program: &Program) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + program.nodes.len() * 64 + program.edges * 8);
        out.extend(b"BLZW");
        out.extend(1u32.to_le_bytes());
        text(&mut out, program.root.to_string_lossy().as_bytes());
        out.extend(program.build.to_le_bytes());
        out.extend((program.crates.len() as u32).to_le_bytes());
        for krate in &program.crates {
            text(&mut out, krate.as_bytes());
        }
        out.extend((program.nodes.len() as u32).to_le_bytes());
        for node in &program.nodes {
            // SAFETY: the world's thread; every node has a Function.
            let function = unsafe {
                &*ecs::ecs_get_id(self.world, node.entity, self.function).cast::<Function>()
            };
            out.extend(function.key.to_le_bytes());
            out.extend(node.krate.to_le_bytes());
            out.push(function.how);
            out.extend(function.ns.to_le_bytes());
            out.extend(function.build.to_le_bytes());
            // SAFETY: Flecs's copy of the name, NUL-terminated.
            text(&mut out, unsafe { CStr::from_ptr(function.name) }.to_bytes());
        }
        out.extend((program.edges as u32).to_le_bytes());
        for (from, calls) in program.out.iter().enumerate() {
            let mut calls = calls.clone();
            calls.sort_unstable();
            for to in calls {
                out.extend((from as u32).to_le_bytes());
                out.extend(to.to_le_bytes());
            }
        }
        out
    }

    fn workspace_of(&mut self, root: &Path, build: u64) -> Entity {
        // SAFETY: the world's thread.
        let entity = unsafe { ecs::ecs_new(self.world) };
        let root = CString::new(root.to_string_lossy().replace('\0', "")).unwrap_or_default();
        let value = Workspace { root: root.as_ptr(), build };
        self.set(entity, self.workspace, &value);
        entity
    }

    fn set_workspace(&mut self, program: &Program) {
        let root = program.root.to_string_lossy().replace('\0', "");
        let root = CString::new(root).unwrap_or_default();
        let value = Workspace { root: root.as_ptr(), build: program.build };
        self.set(program.entity, self.workspace, &value);
    }

    /// A build writes the whole program again: what it says is all of it.
    fn forget_functions(&mut self, program: &mut Program) {
        // SAFETY: the world's thread; children go with the workspace.
        unsafe { ecs::ecs_delete(self.world, program.entity) };
        program.entity = self.workspace_of(&program.root, program.build);
        program.crates.clear();
        program.crates_of.clear();
        program.crate_of.clear();
        program.modules.clear();
        program.nodes.clear();
        program.by_key.clear();
        program.out.clear();
        program.edges = 0;
        program.calls.clear();
        program.module_calls.clear();
        program.unresolved.clear();
    }

    fn placed(
        &mut self,
        program: &mut Program,
        object: u32,
        functions: &[blaze_protocol::events::Function],
    ) {
        let mut placed = Vec::with_capacity(functions.len());
        for function in functions {
            let (krate, in_crate) = self.crate_of(program, &function.krate);
            let at = match program.by_key.get(&function.key) {
                Some(&at) => {
                    // What it calls is said again below.
                    program.nodes[at as usize].krate = krate;
                    program.edges -= program.out[at as usize].len();
                    program.out[at as usize].clear();
                    at
                }
                None => {
                    let parent = self.module_of(program, in_crate, &function.krate, &function.name);
                    // SAFETY: the world's thread.
                    let entity = unsafe { ecs::ecs_new(self.world) };
                    self.set_parent(entity, parent);
                    let at = program.nodes.len() as u32;
                    program.nodes.push(Node { entity, krate, module: parent });
                    program.out.push(Vec::new());
                    program.by_key.insert(function.key, at);
                    at
                }
            };
            let (how, ns) = match function.how {
                How::Compiled { ns } => (0u8, ns),
                How::Unit => (1, 0),
                How::Unchanged => (2, 0),
                How::Stored => (3, 0),
            };
            let name = CString::new(function.name.replace('\0', "")).unwrap_or_default();
            let value =
                Function { key: function.key, name: name.as_ptr(), how, ns, build: program.build };
            self.set(program.nodes[at as usize].entity, self.function, &value);
            program.ids.entry(object).or_default().insert(function.id, at);
            placed.push(at);
        }
        let mut calls = Vec::new();
        let Some(ids) = program.ids.get(&object) else { return };
        for (function, &from) in functions.iter().zip(&placed) {
            for &call in &function.calls {
                match ids.get(&call) {
                    Some(&to) => calls.push((from, to)),
                    None => program.unresolved.push((object, from, call)),
                }
            }
        }
        program.unresolved.retain(|&(of, from, call)| {
            let to = (of == object).then(|| ids.get(&call)).flatten();
            match to {
                Some(&to) => {
                    calls.push((from, to));
                    false
                }
                None => true,
            }
        });
        let mut changed: Vec<u32> = placed;
        let mut changed_modules = Vec::new();
        for (from, to) in calls {
            let out = &mut program.out[from as usize];
            if out.contains(&to) {
                continue;
            }
            out.push(to);
            program.edges += 1;
            changed.push(from);
            let (from, to) = (&program.nodes[from as usize], &program.nodes[to as usize]);
            let crates =
                (program.crates_of[from.krate as usize], program.crates_of[to.krate as usize]);
            // A function at its crate's root is in no module: its calls are its crate's alone.
            let in_modules = from.module != crates.0 && to.module != crates.1;
            if in_modules
                && from.module != to.module
                && program.calls.insert((from.module, to.module))
            {
                program.module_calls.entry(from.module).or_default().push(to.module);
                changed_modules.push(from.module);
            }
            if crates.0 != crates.1 && program.calls.insert(crates) {
                // SAFETY: the world's thread.
                unsafe { ecs::ecs_add_id(self.world, crates.0, pair(self.calls, crates.1)) };
            }
        }
        changed.sort_unstable();
        changed.dedup();
        for at in changed {
            let callees: Vec<Entity> = program.out[at as usize]
                .iter()
                .map(|&to| program.nodes[to as usize].entity)
                .collect();
            self.set_callees(program.nodes[at as usize].entity, callees);
        }
        changed_modules.sort_unstable();
        changed_modules.dedup();
        for module in changed_modules {
            let callees = program.module_calls[&module].clone();
            self.set_callees(module, callees);
        }
    }

    fn set_callees(&mut self, entity: Entity, mut callees: Vec<Entity>) {
        let vector = ecs::ecs_vec_t {
            array: callees.as_mut_ptr().cast::<c_void>(),
            count: callees.len() as i32,
            size: callees.capacity() as i32,
        };
        self.set(entity, self.callees, &vector);
    }

    fn crate_of(&mut self, program: &mut Program, name: &str) -> (u32, Entity) {
        if let Some(&it) = program.crate_of.get(name) {
            return it;
        }
        let entity = self.named(program.entity, name, self.krate);
        let it = (program.crates.len() as u32, entity);
        program.crates.push(name.to_owned());
        program.crates_of.push(entity);
        program.crate_of.insert(name.to_owned(), it);
        it
    }

    /// The module a function is in ([`module_path`]), under its crate, from the crate's name on.
    fn module_of(
        &mut self,
        program: &mut Program,
        mut at: Entity,
        krate: &str,
        name: &str,
    ) -> Entity {
        let mut segments = module_path(name);
        if segments.first() == Some(&krate) {
            segments.remove(0);
        }
        for segment in segments {
            let key = (at, segment.to_owned());
            at = match program.modules.get(&key) {
                Some(&it) => it,
                None => {
                    let it = self.named(at, segment, self.module);
                    program.modules.insert(key, it);
                    it
                }
            };
        }
        at
    }

    fn named(&mut self, parent: Entity, name: &str, kind: Entity) -> Entity {
        let name = CString::new(name.replace('\0', "")).unwrap_or_default();
        // SAFETY: the world's thread.
        let entity = unsafe { ecs::ecs_new(self.world) };
        self.set_parent(entity, parent);
        // SAFETY: as above, and a NUL-terminated name Flecs copies.
        unsafe {
            ecs::ecs_set_name(self.world, entity, name.as_ptr());
            ecs::ecs_add_id(self.world, entity, kind);
        }
        entity
    }

    /// Puts `entity` in `parent` with Flecs's `Parent`, a `ChildOf` that does not fragment:
    /// with `(ChildOf, module)` the functions of each module are a table of their own, and anim
    /// has 5 000 modules.
    fn set_parent(&mut self, entity: Entity, parent: Entity) {
        // SAFETY: a constant Flecs sets once, before any world.
        let id = unsafe { ecs::FLECS_IDEcsParentID_ };
        self.set(entity, id, &Parent { value: parent });
    }

    /// Sets `value` as the component `id` of `entity`: Flecs copies it, its strings too.
    fn set<T>(&mut self, entity: Entity, id: Entity, value: &T) {
        // SAFETY: the world's thread, and `T` is laid out as the component (KINDS).
        unsafe {
            ecs::ecs_set_id(
                self.world,
                entity,
                id,
                size_of::<T>(),
                std::ptr::from_ref(value).cast::<c_void>(),
            )
        };
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // SAFETY: the world's thread, and nothing uses either after.
        unsafe {
            ecs::glue_rest_free(self.rest);
            ecs::ecs_fini(self.world);
        }
    }
}

impl Program {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn build(&self) -> u64 {
        self.build
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    pub fn functions(&self) -> usize {
        self.nodes.len()
    }

    pub fn calls(&self) -> usize {
        self.edges
    }
}

/// The path a function is in, from how a backtrace names it: its path but its last segment,
/// without generic arguments or closures, and of a method of `<T as Trait>` or `<T>`, `T`'s
/// path. As the page's `moduleOf` (assets/names.js), so the world's modules are the page's:
///
/// ```text
/// anim::scene::render                                  anim::scene
/// anim::main::{closure#0}                              anim
/// core::ptr::drop_in_place::<anim::Thing>              core::ptr
/// <alloc::vec::Vec<T> as core::ops::drop::Drop>::drop  alloc::vec::Vec
/// <&[u8] as core::fmt::Debug>::fmt                     (none: a slice has no path)
/// ```
pub fn module_path(name: &str) -> Vec<&str> {
    if name.starts_with('<') {
        return match closing(name, 0) {
            Some(end) => self_path(&name[1..end]),
            None => Vec::new(),
        };
    }
    let mut segments = segments(name);
    segments.pop();
    segments
}

/// `name` split at each `::` outside `<>`, `()`, `[]` and `{}`.
fn split_path(name: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start, mut last) = (Vec::new(), 0usize, 0, '\0');
    let mut chars = name.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' if last != '-' => depth = depth.saturating_sub(1),
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ':' if depth == 0 && chars.peek().is_some_and(|&(_, next)| next == ':') => {
                out.push(&name[start..at]);
                chars.next();
                start = at + 2;
            }
            _ => {}
        }
        last = c;
    }
    out.push(&name[start..]);
    out
}

/// The byte of the `>` that closes the `<` at `open`.
fn closing(name: &str, open: usize) -> Option<usize> {
    let (mut depth, mut last) = (0usize, '\0');
    for (at, c) in name[open..].char_indices() {
        if c == '<' {
            depth += 1;
        } else if c == '>' && last != '-' {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(open + at);
            }
        }
        last = c;
    }
    None
}

/// The path of `T` in `T as Trait`, or of `T`.
fn self_path(inner: &str) -> Vec<&str> {
    let (mut depth, mut last) = (0usize, '\0');
    for (at, c) in inner.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' if last != '-' => depth = depth.saturating_sub(1),
            ')' | ']' => depth = depth.saturating_sub(1),
            ' ' if depth == 0 && inner[at..].starts_with(" as ") => return type_path(&inner[..at]),
            _ => {}
        }
        last = c;
    }
    type_path(inner)
}

/// A type's path without its generic arguments, or none for a type that has none: a reference
/// or pointer is its pointee's, a slice, tuple or `fn` pointer has none.
fn type_path(ty: &str) -> Vec<&str> {
    let mut ty = ty.trim();
    loop {
        let before = ty;
        for prefix in ["&mut ", "&", "*const ", "*mut ", "dyn "] {
            ty = ty.strip_prefix(prefix).unwrap_or(ty).trim_start();
        }
        if let Some(rest) = ty.strip_prefix('\'') {
            ty = rest.split_once(' ').map_or(ty, |(_, rest)| rest.trim_start());
        }
        if ty == before {
            break;
        }
    }
    if ty.starts_with('<') {
        return match closing(ty, 0) {
            Some(end) => self_path(&ty[1..end]),
            None => Vec::new(),
        };
    }
    let fn_pointer =
        ty.starts_with("fn(") || ty.starts_with("unsafe ") || ty.starts_with("extern ");
    if fn_pointer || !ty.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        return Vec::new();
    }
    segments(ty)
}

/// The named segments of a path: no `::<…>`, generic arguments or `{closure#0}`.
fn segments(path: &str) -> Vec<&str> {
    split_path(path)
        .into_iter()
        .filter(|it| !it.is_empty() && !it.starts_with('<') && !it.starts_with('{'))
        .map(|it| it.find('<').map_or(it, |lt| &it[..lt]))
        .collect()
}

fn pair(relationship: Entity, target: Entity) -> ecs::ecs_id_t {
    ecs::ECS_PAIR | ((relationship & 0xFFFF_FFFF) << 32) | (target & 0xFFFF_FFFF)
}

fn text(out: &mut Vec<u8>, text: &[u8]) {
    out.extend((text.len() as u32).to_le_bytes());
    out.extend(text);
}
