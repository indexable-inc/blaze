//! The functions of each workspace's program as a Flecs world, as its builds' records say them,
//! the snapshot of it a page is sent first, and Flecs's REST over it (docs/web.md).
//!
//! What it holds:
//!
//! - a workspace: [`Workspace`], unnamed; under it (`ChildOf`) each crate the build put a
//!   function of, `Crate`, named as the crate, and under a crate its modules, `Module`, named as
//!   the module, one in another as in the crate;
//! - a function: [`Function`], unnamed, in (`ChildOf`) the module its path names, or the crate
//!   when it names none, and `(Calls, function)` for each function it calls.
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
Crate {}
Module {}
Calls { Relationship\n DontFragment }
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
    crate_of: FxHashMap<String, (u32, Entity)>,
    modules: FxHashMap<(Entity, String), Entity>,
    /// Each function's entity and crate, in the snapshot's order.
    nodes: Vec<(Entity, u32)>,
    by_key: FxHashMap<u64, u32>,
    /// The nodes of each object of the build under way, by their ids in it.
    ids: FxHashMap<u32, FxHashMap<u32, u32>>,
    /// The nodes each node calls.
    out: Vec<Vec<u32>>,
    edges: usize,
    /// Calls of the build's objects to ids none of its records has named yet: the object,
    /// the node that calls and the id. One to code of another is never named.
    unresolved: Vec<(u32, u32, u32)>,
    /// The records of the last build, but for its functions.
    records: Vec<Record>,
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
            if ecs::ecs_script_run(world, c"blaze-web".as_ptr(), code.as_ptr(), std::ptr::null_mut())
                != 0
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
            crate_of: FxHashMap::default(),
            modules: FxHashMap::default(),
            nodes: Vec::new(),
            by_key: FxHashMap::default(),
            ids: FxHashMap::default(),
            out: Vec::new(),
            edges: 0,
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
        for &(entity, krate) in &program.nodes {
            // SAFETY: the world's thread; every node has a Function.
            let function = unsafe {
                &*ecs::ecs_get_id(self.world, entity, self.function).cast::<Function>()
            };
            out.extend(function.key.to_le_bytes());
            out.extend(krate.to_le_bytes());
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
        program.crate_of.clear();
        program.modules.clear();
        program.nodes.clear();
        program.by_key.clear();
        program.out.clear();
        program.edges = 0;
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
                    let entity = program.nodes[at as usize].0;
                    // What it calls is said again below.
                    // SAFETY: the world's thread.
                    unsafe { ecs::ecs_remove_id(self.world, entity, pair(self.calls, wildcard())) };
                    program.nodes[at as usize].1 = krate;
                    program.edges -= program.out[at as usize].len();
                    program.out[at as usize].clear();
                    at
                }
                None => {
                    let parent = self.module_of(program, in_crate, &function.krate, &function.name);
                    // SAFETY: the world's thread.
                    let entity = unsafe { ecs::ecs_new(self.world) };
                    // SAFETY: as above.
                    unsafe { ecs::ecs_add_id(self.world, entity, pair(child_of(), parent)) };
                    let at = program.nodes.len() as u32;
                    program.nodes.push((entity, krate));
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
            self.set(program.nodes[at as usize].0, self.function, &value);
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
        for (from, to) in calls {
            let out = &mut program.out[from as usize];
            if !out.contains(&to) {
                out.push(to);
                program.edges += 1;
                let (from, to) = (program.nodes[from as usize].0, program.nodes[to as usize].0);
                // SAFETY: the world's thread.
                unsafe { ecs::ecs_add_id(self.world, from, pair(self.calls, to)) };
            }
        }
    }

    fn crate_of(&mut self, program: &mut Program, name: &str) -> (u32, Entity) {
        if let Some(&it) = program.crate_of.get(name) {
            return it;
        }
        let entity = self.named(program.entity, name, self.krate);
        let it = (program.crates.len() as u32, entity);
        program.crates.push(name.to_owned());
        program.crate_of.insert(name.to_owned(), it);
        it
    }

    /// The module a function's path names in its crate: the segments of its path before its
    /// own, from the crate's on, that are a module's (begin with a lowercase letter).
    fn module_of(&mut self, program: &mut Program, mut at: Entity, krate: &str, name: &str) -> Entity {
        let path = path_of(name);
        let mut segments: Vec<&str> = path.split("::").collect();
        segments.pop();
        if segments.first() == Some(&krate) {
            segments.remove(0);
        }
        for segment in segments {
            if !segment.starts_with(|c: char| c.is_ascii_lowercase()) {
                break;
            }
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
        // SAFETY: the world's thread, a NUL-terminated name Flecs copies.
        unsafe {
            let entity = ecs::ecs_new(self.world);
            ecs::ecs_add_id(self.world, entity, pair(child_of(), parent));
            ecs::ecs_set_name(self.world, entity, name.as_ptr());
            ecs::ecs_add_id(self.world, entity, kind);
            entity
        }
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

/// A function's path as a backtrace names it, without its generic arguments, and `<T>::f` as
/// `T::f`: `<alloc::vec::Vec<u8>>::push` is `alloc::vec::Vec::push`.
pub fn path_of(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c != '<' || at == 0 {
            out.push(c);
            continue;
        }
        if out.ends_with("::") {
            out.truncate(out.len() - 2);
        }
        let mut depth = 1;
        for (_, c) in chars.by_ref() {
            depth += match c {
                '<' => 1,
                '>' => -1,
                _ => 0,
            };
            if depth == 0 {
                break;
            }
        }
    }
    if let Some(rest) = out.strip_prefix('<')
        && let Some(end) = rest.find(">::")
        && !rest[..end].contains(" as ")
    {
        return format!("{}{}", &rest[..end], &rest[end + 1..]);
    }
    out
}

fn pair(relationship: Entity, target: Entity) -> ecs::ecs_id_t {
    ecs::ECS_PAIR | ((relationship & 0xFFFF_FFFF) << 32) | (target & 0xFFFF_FFFF)
}

fn child_of() -> Entity {
    // SAFETY: a constant Flecs sets once, before any world.
    unsafe { ecs::EcsChildOf }
}

fn wildcard() -> Entity {
    // SAFETY: as `child_of`.
    unsafe { ecs::EcsWildcard }
}

fn text(out: &mut Vec<u8>, text: &[u8]) {
    out.extend((text.len() as u32).to_le_bytes());
    out.extend(text);
}
