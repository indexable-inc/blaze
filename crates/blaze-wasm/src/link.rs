//! Makes WebAssembly modules out of what each build compiled, one module per build.
//!
//! The first module and every one after it share one memory, one function table and one
//! stack pointer, which they import. Each function has a slot in the table for as long as
//! the page lives, by what it is across edits of its body ([`blaze_codegen::exec::fn_id`]),
//! and what calls it through a pointer calls its slot. So a build after an edit is a module
//! with only the functions whose code or whose places changed, and instantiating it writes
//! their slots: what called the old code calls the new code, and the program's memory stays
//! what it was. A function an earlier module has that one of this module calls directly is
//! imported from the page by its slot ([`SLOT`]), so its caller is put again whenever it is.

use blaze_codegen::{
    Arch, CompiledFunction, Data, Init, InstanceKey, RelocKind, Relocs, Static, Target,
    driver::Built,
    wasm::{Code, Import, Signature, ValType},
};
use blaze_format::{SpanRef, leb};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, ElementSection, Elements, EntityType, FunctionSection,
    GlobalType, ImportSection, MemoryType, Module, NameMap, NameSection, RefType, TableType,
    TypeSection,
};

#[cfg(test)]
mod tests;

/// The module the memory, the table and the stack pointer are imported from.
pub const ENV: &str = "env";
/// The module the functions of earlier modules are imported from, each by its slot as a
/// decimal number: the page gives the function in that slot of the table.
pub const SLOT: &str = "slot";

/// Nothing is put below this, so that null and what is near it stay unmapped data.
const STACK_START: u64 = 1 << 16;
const STACK_SIZE: u64 = 1 << 20;
/// A function index, a slot or an address in code: a padded LEB128 of five bytes.
const INDEX_LEN: usize = 5;
/// `core::panic::Location<'static>` on wasm32: the file name's pointer and length, then
/// `line` and `col`, each four bytes.
const LOCATION_SIZE: u64 = 16;
const LOCATION_ALIGN: u32 = 4;
const LINKED_DOMAIN: &str = "blaze-wasm linked function 1";
const STATIC_DOMAIN: &str = "blaze-wasm static content 1";

/// Where everything linked so far is.
pub struct Linker {
    /// The slot of each function by what it is across versions, with what was last put there.
    slots: HashMap<ContentId, Slot>,
    /// The slot of each imported function something points to, by its module and field.
    imported: HashMap<(String, String), u32>,
    statics: HashMap<(ItemKey, ContentId), Placed>,
    /// Read-only data, by content: placed once, never moved.
    data: HashMap<ContentId, u64>,
    /// The `Location` of each position code named, and each file name they point at.
    positions: HashMap<SpanRef, u64>,
    files: HashMap<String, u64>,
    table_len: u32,
    memory_len: u64,
}

struct Slot {
    index: u32,
    /// What was last put there ([`Linker::linked`]), `None` until something is.
    linked: Option<ContentId>,
}

struct Placed {
    address: u64,
    size: u64,
    content: ContentId,
}

/// A module to instantiate, and what it needs to be there first.
#[derive(Debug)]
pub struct Patch {
    pub module: Vec<u8>,
    /// How many slots the table has to have, and how many bytes the memory.
    pub table_len: u32,
    pub memory_len: u64,
    /// The stack pointer to start with, for the first module.
    pub stack_top: u64,
    /// The slots of the functions of the program that can be called by name.
    pub exports: Vec<(String, u32)>,
    /// The names of the functions in the module, and the bytes of data in it.
    pub functions: Vec<String>,
    pub data_len: usize,
}

impl Default for Linker {
    fn default() -> Self {
        Linker {
            slots: HashMap::default(),
            imported: HashMap::default(),
            statics: HashMap::default(),
            data: HashMap::default(),
            positions: HashMap::default(),
            files: HashMap::default(),
            // Slot 0 stays empty: a null function pointer traps.
            table_len: 1,
            memory_len: STACK_START + STACK_SIZE,
        }
    }
}

/// One function of the build as this link reads it.
struct Function<'a> {
    compiled: &'a CompiledFunction,
    code: Code,
    /// Where the body starts in `compiled.code`.
    start: usize,
    /// What it is across versions, its slot's key.
    id: ContentId,
}

/// The build's span tables, decoded as positions ask for them.
struct Spans<'a> {
    encoded: HashMap<ContentId, &'a [u8]>,
    decoded: HashMap<ContentId, SpanTable>,
}

/// One span table, as `blaze_link::SpanTable` encodes it.
struct SpanTable {
    files: Vec<String>,
    /// By the span's index: its file (an index into `files`), line and column.
    spans: Vec<[u32; 3]>,
}

/// What goes in the module of one build.
#[derive(Default)]
struct Output {
    types: Vec<Signature>,
    type_of: HashMap<Signature, u32>,
    /// Module, field and type of each function imported, in index order.
    imports: Vec<(String, String, u32)>,
    import_of: HashMap<(String, String), u32>,
    /// Type, body and name of each function defined, in index order after the imports.
    defined: Vec<(u32, Vec<u8>, String)>,
    /// What the element segments put in the table: slot and function index.
    elements: Vec<(u32, u32)>,
    data: Vec<(u64, Vec<u8>)>,
}

impl Output {
    fn ty(&mut self, signature: &Signature) -> u32 {
        if let Some(&at) = self.type_of.get(signature) {
            return at;
        }
        let at = self.types.len() as u32;
        self.types.push(signature.clone()); // clone: once a signature new to the module
        self.type_of.insert(signature.clone(), at); // clone: as above
        at
    }

    /// The index of the function imported as `module` and `field`.
    fn import(&mut self, module: &str, field: &str, ty: &Signature) -> u32 {
        let key = (module.to_owned(), field.to_owned()); // alloc: once an import of the module
        if let Some(&at) = self.import_of.get(&key) {
            return at;
        }
        let ty = self.ty(ty);
        let at = self.imports.len() as u32;
        self.imports.push((key.0.clone(), key.1.clone(), ty)); // clone: as above
        self.import_of.insert(key, at);
        at
    }
}

fn hashed(hasher: &mut Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

impl Linker {
    /// Links what `built` compiled against everything linked before: a module of what is new.
    /// `spans` are the build's span tables by anchor, which its positions are read from.
    pub fn link(&mut self, built: &Built, spans: &[(ContentId, Vec<u8>)]) -> Result<Patch, String> {
        let ids: HashMap<InstanceKey, ContentId> = built
            .inputs
            .iter()
            .zip(&built.versions)
            .map(|((key, _), (id, _))| (*key, *id))
            .collect();
        let kept = |key: &InstanceKey| built.deduped.merged.get(key).copied().unwrap_or(*key);
        let mut functions = Vec::with_capacity(built.functions.len());
        let mut by_key = HashMap::default();
        for compiled in &built.functions {
            if compiled.arch != Arch::Wasm32 {
                return Err(format!(
                    "{:?}: code for {:?}, where a page runs wasm32",
                    compiled.key, compiled.arch
                ));
            }
            let (code, start) = Code::decode(&compiled.code)?;
            let id = *ids
                .get(&compiled.key)
                .ok_or_else(|| format!("{:?}: a function the walk did not reach", compiled.key))?;
            by_key.insert(compiled.key, functions.len());
            functions.push(Function { compiled, code, start, id });
        }
        for function in &functions {
            self.slot(function.id);
        }
        let mut externs: HashMap<&str, &Import> = HashMap::default();
        for function in &functions {
            externs
                .extend(function.code.imports.iter().map(|(name, import)| (name.as_str(), import)));
        }
        for s in &built.statics {
            externs.extend(s.imports.iter().map(|(name, import)| (name.as_str(), import)));
        }
        let mut spans = Spans {
            encoded: spans.iter().map(|(id, bytes)| (*id, bytes.as_slice())).collect(),
            decoded: HashMap::default(),
        };
        let mut out = Output::default();

        // Data first, then statics: neither has an address that depends on what is in it.
        let mut new_data = Vec::new();
        let all_data = functions
            .iter()
            .flat_map(|it| &it.compiled.data)
            .chain(built.statics.iter().flat_map(|it| &it.data));
        for data in all_data {
            if !self.data.contains_key(&data.id) {
                let address = self.alloc(data.align, data.bytes.len() as u64)?;
                self.data.insert(data.id, address);
                new_data.push(data);
            }
        }
        let mut written_statics = Vec::new();
        let mut moved = HashSet::default();
        for s in &built.statics {
            if let Some(section) = &s.section {
                return Err(format!(
                    "{:?}: a static in `#[link_section = \"{section}\"]`, which a page's module has no place for",
                    s.item
                ));
            }
            let key = (s.item, s.instance);
            let size = match &s.init {
                Init::Bytes { bytes, .. } => bytes.len() as u64,
                Init::Zeroed { size } => *size,
            };
            let content = self.static_content(s, &ids, &kept)?;
            match self.statics.get_mut(&key) {
                Some(placed) if placed.content == content => {}
                Some(placed)
                    if size <= placed.size && placed.address % u64::from(s.align.max(1)) == 0 =>
                {
                    placed.content = content;
                    written_statics.push(s);
                }
                Some(_) => {
                    let address = self.alloc(s.align, size)?;
                    self.statics.insert(key, Placed { address, size, content });
                    moved.insert(key);
                    written_statics.push(s);
                }
                None => {
                    let address = self.alloc(s.align, size)?;
                    self.statics.insert(key, Placed { address, size, content });
                    if matches!(s.init, Init::Bytes { .. }) {
                        written_statics.push(s);
                    }
                }
            }
        }
        // What holds the address of a static that moved is written again.
        let points_to_moved = |relocs: &Relocs| {
            relocs.iter().any(|it| matches!(it.target, Target::Static { item, instance } if moved.contains(&(*item, *instance))))
        };
        if !moved.is_empty() {
            let mut seen: HashSet<ContentId> = new_data.iter().map(|it| it.id).collect();
            let all_data = functions
                .iter()
                .flat_map(|it| &it.compiled.data)
                .chain(built.statics.iter().flat_map(|it| &it.data));
            let again: Vec<&Data> =
                all_data.filter(|it| points_to_moved(&it.relocs) && seen.insert(it.id)).collect();
            new_data.extend(again);
            for s in &built.statics {
                if let Init::Bytes { relocs, .. } = &s.init
                    && points_to_moved(relocs)
                    && !written_statics
                        .iter()
                        .any(|it| (it.item, it.instance) == (s.item, s.instance))
                {
                    written_statics.push(s);
                }
            }
        }
        for data in &new_data {
            let address = self.data[&data.id];
            let bytes = self.data_bytes(
                &data.bytes,
                &data.relocs,
                &ids,
                &kept,
                &externs,
                &mut spans,
                &mut out,
            )?;
            out.data.push((address, bytes));
        }
        for s in &written_statics {
            let address = self.statics[&(s.item, s.instance)].address;
            let bytes = match &s.init {
                Init::Bytes { bytes, relocs } => {
                    self.data_bytes(bytes, relocs, &ids, &kept, &externs, &mut spans, &mut out)?
                }
                Init::Zeroed { size } => vec![0; *size as usize],
            };
            out.data.push((address, bytes));
        }

        // What each function links to, and so which are put in this module.
        let mut linked = Vec::with_capacity(functions.len());
        for function in &functions {
            linked.push(self.linked(function, &ids, &kept, &externs, &mut spans, &mut out)?);
        }
        let mut put: Vec<bool> = functions
            .iter()
            .zip(&linked)
            .map(|(it, linked)| self.slots[&it.id].linked != Some(*linked))
            .collect();
        // A direct call binds the function an import gave when its caller's module was made.
        let mut callers: HashMap<usize, Vec<usize>> = HashMap::default();
        for (at, function) in functions.iter().enumerate() {
            for reloc in function.compiled.relocs.iter() {
                if reloc.kind == RelocKind::WasmFunction
                    && let Target::Function(key) | Target::Direct(key) = reloc.target
                    && let Some(&callee) = by_key.get(&kept(key))
                {
                    callers.entry(callee).or_default().push(at);
                }
            }
        }
        let mut pending: Vec<usize> = (0..functions.len()).filter(|&at| put[at]).collect();
        while let Some(callee) = pending.pop() {
            for &caller in callers.get(&callee).map_or(&[][..], Vec::as_slice) {
                if !put[caller] {
                    put[caller] = true;
                    pending.push(caller);
                }
            }
        }

        // Every function this module defines is imported after the imports it calls; the
        // indices of those are only known once all are listed.
        let order: Vec<usize> = (0..functions.len()).filter(|&at| put[at]).collect();
        let mut bodies = Vec::with_capacity(order.len());
        let mut calls = Vec::with_capacity(order.len());
        for &at in &order {
            let function = &functions[at];
            let mut direct = Vec::new();
            for reloc in function.compiled.relocs.iter() {
                if reloc.kind != RelocKind::WasmFunction {
                    continue;
                }
                let callee = match reloc.target {
                    Target::Function(key) | Target::Direct(key) => {
                        let callee = *by_key.get(&kept(key)).ok_or_else(|| {
                            format!(
                                "{:?} calls {key:?}, which the build has no code of",
                                function.compiled.key
                            )
                        })?;
                        match put[callee] {
                            true => Callee::Defined(callee),
                            false => {
                                let slot = self.slots[&functions[callee].id].index;
                                Callee::Import(out.import(
                                    SLOT,
                                    &slot.to_string(),
                                    &functions[callee].code.ty,
                                ))
                            }
                        }
                    }
                    Target::Extern(name) => {
                        let import = externs.get(name.as_str()).ok_or_else(|| {
                            format!("{:?} calls `{name}`, which nothing defines and no `#[link(wasm_import_module)]` names", function.compiled.key)
                        })?;
                        Callee::Import(out.import(&import.module, &import.field, &import.ty))
                    }
                    other => {
                        return Err(format!(
                            "{:?}: a call to {other:?}, which is no function",
                            function.compiled.key
                        ));
                    }
                };
                direct.push((reloc.at, callee));
            }
            calls.push(direct);
            bodies.push(self.body(function, &ids, &kept, &externs, &mut spans, &mut out)?);
        }
        let names = names(built);
        let first_defined = out.imports.len() as u32;
        let index_of: HashMap<usize, u32> = order
            .iter()
            .enumerate()
            .map(|(place, &at)| (at, first_defined + place as u32))
            .collect();
        for ((&at, mut body), direct) in order.iter().zip(bodies).zip(calls) {
            let function = &functions[at];
            for (reloc_at, callee) in direct {
                let index = match callee {
                    Callee::Defined(callee) => index_of[&callee],
                    Callee::Import(index) => index,
                };
                let place = (reloc_at as usize)
                    .checked_sub(function.start)
                    .ok_or("a relocation in a code header")?;
                put_index(&mut body, place, u64::from(index))?;
            }
            let ty = out.ty(&function.code.ty);
            let slot = self.slots[&function.id].index;
            out.elements.push((slot, index_of[&at]));
            let name = match names.get(&function.compiled.key) {
                Some(name) => (*name).to_owned(),
                None => format!("{:?}", function.compiled.key.0),
            };
            out.defined.push((ty, body, name));
            self.slots.get_mut(&function.id).expect("given a slot above").linked = Some(linked[at]);
        }

        let mut exports: Vec<(String, u32)> = Vec::new();
        for (key, name) in &built.symbols {
            if let Some(&at) = by_key.get(&kept(key)) {
                exports.push((name.clone(), self.slots[&functions[at].id].index));
            }
        }
        for function in &functions {
            if let Some(name) = &function.code.export
                && !exports.iter().any(|it| it.0 == *name)
            {
                exports.push((name.clone(), self.slots[&function.id].index));
            }
        }
        exports.sort_unstable();
        Ok(Patch {
            data_len: out.data.iter().map(|it| it.1.len()).sum(),
            functions: out.defined.iter().map(|it| it.2.clone()).collect(),
            module: out.module(),
            table_len: self.table_len,
            memory_len: self.memory_len,
            stack_top: STACK_START + STACK_SIZE,
            exports,
        })
    }

    /// Links into the memory and table of a module that is there already, from the first
    /// slot of its table and the first byte of its memory that are free for what is linked.
    /// Only before the first link: what is linked stays where it was put.
    pub fn set_start(&mut self, table_start: u32, memory_start: u64) -> Result<(), String> {
        if !self.slots.is_empty() || !self.data.is_empty() || !self.statics.is_empty() {
            return Err("where the program goes is set before its first build".to_owned());
        }
        self.table_len = table_start.max(1);
        self.memory_len = memory_start;
        Ok(())
    }

    fn slot(&mut self, id: ContentId) -> u32 {
        if let Some(slot) = self.slots.get(&id) {
            return slot.index;
        }
        let index = self.table_len;
        self.table_len += 1;
        self.slots.insert(id, Slot { index, linked: None });
        index
    }

    fn alloc(&mut self, align: u32, size: u64) -> Result<u64, String> {
        let address = self.memory_len.next_multiple_of(u64::from(align.max(1)));
        let end = address
            .checked_add(size)
            .filter(|&end| end <= 1 << 32)
            .ok_or("the program's memory past 4 GiB")?;
        self.memory_len = end;
        Ok(address)
    }

    /// What a static is, its places by what they name rather than where that is: what tells
    /// one that has to be written again from one that is as it was.
    fn static_content(
        &self,
        s: &Static,
        ids: &HashMap<InstanceKey, ContentId>,
        kept: &impl Fn(&InstanceKey) -> InstanceKey,
    ) -> Result<ContentId, String> {
        let mut hasher = Hasher::new(STATIC_DOMAIN);
        hashed(&mut hasher, u64::from(s.align));
        hasher.update(&[u8::from(s.mutable), u8::from(s.thread_local)]);
        match &s.init {
            Init::Zeroed { size } => {
                hasher.update(&[0]);
                hashed(&mut hasher, *size);
            }
            Init::Bytes { bytes, relocs } => {
                hasher.update(&[1]);
                hashed(&mut hasher, bytes.len() as u64);
                hasher.update(bytes);
                for reloc in relocs.iter() {
                    hashed(&mut hasher, u64::from(reloc.at));
                    hashed(&mut hasher, reloc.addend as u64);
                    hasher.update(&[reloc.kind as u8]);
                    match reloc.target {
                        Target::Function(key) | Target::Direct(key) => {
                            let id = ids.get(&kept(key)).ok_or_else(|| {
                                format!(
                                    "{:?} points to {key:?}, which the walk did not reach",
                                    s.item
                                )
                            })?;
                            hasher.update(&[0]).id(id);
                        }
                        Target::Static { item, instance } => {
                            hasher.update(&[1]).id(&item.0).id(instance);
                        }
                        Target::Data(id) => {
                            hasher.update(&[2]).id(id);
                        }
                        Target::Extern(name) | Target::Weak(name) => {
                            hasher.update(&[3]).update(name.as_bytes());
                        }
                        Target::Position(span) => {
                            hasher.update(&[4]).id(&span.anchor);
                            hashed(&mut hasher, u64::from(span.index));
                        }
                        other => {
                            return Err(format!(
                                "{:?}: a static points to {other:?}, which a page's program has none of",
                                s.item
                            ));
                        }
                    }
                }
            }
        }
        Ok(hasher.finish())
    }

    /// The value a relocation of `kind` to `target` is, but a direct call: the slot of a
    /// function, the address of a static, data or a position.
    #[allow(clippy::too_many_arguments)]
    fn value(
        &mut self,
        target: &Target,
        kind: RelocKind,
        ids: &HashMap<InstanceKey, ContentId>,
        kept: &impl Fn(&InstanceKey) -> InstanceKey,
        externs: &HashMap<&str, &Import>,
        spans: &mut Spans<'_>,
        out: &mut Output,
    ) -> Result<u64, String> {
        match (kind, target) {
            (RelocKind::WasmTable, Target::Function(key) | Target::Direct(key)) => {
                let id = ids.get(&kept(key)).ok_or_else(|| format!("a pointer to {key:?}, which the walk did not reach"))?;
                Ok(u64::from(self.slot(*id)))
            }
            (RelocKind::WasmTable, Target::Extern(name) | Target::Weak(name)) => match externs.get(name.as_str()) {
                Some(import) => Ok(u64::from(self.import_slot(import, out))),
                None if matches!(target, Target::Weak(_)) => Ok(0),
                None => Err(format!("a pointer to `{name}`, which nothing defines and no `#[link(wasm_import_module)]` names")),
            },
            (RelocKind::WasmMemory, Target::Static { item, instance }) => match self.statics.get(&(*item, *instance)) {
                Some(placed) => Ok(placed.address),
                None => Err(format!("the address of the static {item:?}, which the build does not have")),
            },
            (RelocKind::WasmMemory, Target::Data(id)) => {
                self.data.get(id).copied().ok_or_else(|| format!("the address of the data {id:?}, which no function or static of the build has"))
            }
            (RelocKind::WasmMemory, Target::Position(span)) => self.position(*span, spans, out),
            (RelocKind::WasmMemory, Target::Weak(_)) => Ok(0),
            (RelocKind::WasmMemory, Target::Extern(name)) => {
                Err(format!("the address of `{name}`, data that nothing in the program defines: a page's module imports no data"))
            }
            (kind, target) => Err(format!("a {kind:?} relocation to {target:?}, which a page's module has no place for")),
        }
    }

    /// The slot of an imported function something points to: the module that first does
    /// imports it and puts it there.
    fn import_slot(&mut self, import: &Import, out: &mut Output) -> u32 {
        let key = (import.module.clone(), import.field.clone()); // clone: once a pointer to an import
        if let Some(&slot) = self.imported.get(&key) {
            return slot;
        }
        let slot = self.table_len;
        self.table_len += 1;
        self.imported.insert(key, slot);
        let index = out.import(&import.module, &import.field, &import.ty);
        out.elements.push((slot, index));
        slot
    }

    /// The address of the `Location` of `span`, placed and written the first time.
    fn position(
        &mut self,
        span: SpanRef,
        spans: &mut Spans<'_>,
        out: &mut Output,
    ) -> Result<u64, String> {
        if let Some(&at) = self.positions.get(&span) {
            return Ok(at);
        }
        let (file, line, column) = spans.position(span)?;
        let name = match self.files.get(&file) {
            Some(&at) => at,
            None => {
                let at = self.alloc(1, file.len() as u64 + 1)?;
                // NUL-terminated: `Location::file_as_c_str` reads one byte past the length.
                let mut bytes = Vec::with_capacity(file.len() + 1);
                bytes.extend_from_slice(file.as_bytes());
                bytes.push(0);
                out.data.push((at, bytes));
                self.files.insert(file.clone(), at); // clone: once a file name
                at
            }
        };
        let at = self.alloc(LOCATION_ALIGN, LOCATION_SIZE)?;
        let mut record = Vec::with_capacity(LOCATION_SIZE as usize);
        for word in [name as u32, file.len() as u32, line, column] {
            record.extend_from_slice(&word.to_le_bytes());
        }
        out.data.push((at, record));
        self.positions.insert(span, at);
        Ok(at)
    }

    /// Data's bytes, each place in them holding the 4-byte address or slot it names.
    #[allow(clippy::too_many_arguments)]
    fn data_bytes(
        &mut self,
        bytes: &[u8],
        relocs: &Relocs,
        ids: &HashMap<InstanceKey, ContentId>,
        kept: &impl Fn(&InstanceKey) -> InstanceKey,
        externs: &HashMap<&str, &Import>,
        spans: &mut Spans<'_>,
        out: &mut Output,
    ) -> Result<Vec<u8>, String> {
        let mut bytes = bytes.to_vec(); // alloc: the bytes of a data segment
        for reloc in relocs.iter() {
            let value = self.value(reloc.target, reloc.kind, ids, kept, externs, spans, out)?;
            let value = (value as i64).wrapping_add(reloc.addend);
            let value =
                u32::try_from(value).map_err(|_| format!("a 4-byte place holding {value}"))?;
            let at = reloc.at as usize;
            bytes
                .get_mut(at..at + 4)
                .ok_or("a relocation outside its data")?
                .copy_from_slice(&value.to_le_bytes());
        }
        Ok(bytes)
    }

    /// What a function links to: its code, and the slot, address or callee each of its
    /// places names. A function whose slot has this has nothing new to put there.
    #[allow(clippy::too_many_arguments)]
    fn linked(
        &mut self,
        function: &Function<'_>,
        ids: &HashMap<InstanceKey, ContentId>,
        kept: &impl Fn(&InstanceKey) -> InstanceKey,
        externs: &HashMap<&str, &Import>,
        spans: &mut Spans<'_>,
        out: &mut Output,
    ) -> Result<ContentId, String> {
        let mut hasher = Hasher::new(LINKED_DOMAIN);
        hasher.id(&function.compiled.key.0);
        for reloc in function.compiled.relocs.iter() {
            hashed(&mut hasher, u64::from(reloc.at));
            match (reloc.kind, reloc.target) {
                (RelocKind::WasmFunction, Target::Function(key) | Target::Direct(key)) => {
                    let id = ids.get(&kept(key)).ok_or_else(|| {
                        format!(
                            "{:?} calls {key:?}, which the walk did not reach",
                            function.compiled.key
                        )
                    })?;
                    hasher.update(&[0]).id(id);
                }
                (RelocKind::WasmFunction, Target::Extern(name)) => {
                    hasher.update(&[1]).update(name.as_bytes());
                }
                (kind, target) => {
                    let value = self.value(target, kind, ids, kept, externs, spans, out)?;
                    hasher.update(&[2]);
                    hashed(&mut hasher, value.wrapping_add(reloc.addend as u64));
                }
            }
        }
        Ok(hasher.finish())
    }

    /// The body of a function put in this module: its places filled, but for its direct
    /// calls, whose indices are known once every function of the module is listed.
    #[allow(clippy::too_many_arguments)]
    fn body(
        &mut self,
        function: &Function<'_>,
        ids: &HashMap<InstanceKey, ContentId>,
        kept: &impl Fn(&InstanceKey) -> InstanceKey,
        externs: &HashMap<&str, &Import>,
        spans: &mut Spans<'_>,
        out: &mut Output,
    ) -> Result<Vec<u8>, String> {
        let mut body = function.compiled.code[function.start..].to_vec(); // alloc: a function's body, once it is put
        for reloc in function.compiled.relocs.iter() {
            if reloc.kind == RelocKind::WasmFunction {
                continue;
            }
            let value = self.value(reloc.target, reloc.kind, ids, kept, externs, spans, out)?;
            let place = (reloc.at as usize)
                .checked_sub(function.start)
                .ok_or("a relocation in a code header")?;
            let value = (value as i64).wrapping_add(reloc.addend);
            let slice =
                body.get_mut(place..place + INDEX_LEN).ok_or("a relocation outside the body")?;
            leb::put_padded_i64(slice, value)
                .map_err(|_| format!("a relocated value {value} past its {INDEX_LEN} bytes"))?;
        }
        for &(at, which) in &function.code.type_fixups {
            let ty = function
                .code
                .types
                .get(which as usize)
                .ok_or("a type fixup naming no type of the function")?;
            let ty = out.ty(ty);
            put_index(&mut body, at as usize, u64::from(ty))?;
        }
        // The stack pointer is the module's first global, which it imports.
        for &at in &function.code.stack_pointer {
            put_index(&mut body, at as usize, 0)?;
        }
        Ok(body)
    }
}

enum Callee {
    /// A function this module defines, by its place among the build's functions.
    Defined(usize),
    /// An imported function, by its index.
    Import(u32),
}

fn put_index(body: &mut [u8], at: usize, value: u64) -> Result<(), String> {
    let place = body.get_mut(at..at + INDEX_LEN).ok_or("a place outside the body")?;
    leb::put_padded_u64(place, value)
        .map_err(|_| format!("an index {value} past its {INDEX_LEN} bytes"))
}

/// What each function is called in the report and a trap: its symbol, or its name for a
/// debugger.
fn names(built: &Built) -> HashMap<InstanceKey, &str> {
    let mut names: HashMap<InstanceKey, &str> =
        built.debug_names.iter().map(|(key, name)| (*key, name.as_str())).collect();
    names.extend(built.symbols.iter().map(|(key, name)| (*key, name.as_str())));
    names
}

impl Spans<'_> {
    /// The file, line and column of `span`.
    fn position(&mut self, span: SpanRef) -> Result<(String, u32, u32), String> {
        if !self.decoded.contains_key(&span.anchor) {
            let bytes = self.encoded.get(&span.anchor).ok_or_else(|| {
                format!("a position in {:?}, whose span table the build did not give", span.anchor)
            })?;
            self.decoded.insert(span.anchor, SpanTable::decode(bytes)?);
        }
        let table = &self.decoded[&span.anchor];
        let [file, line, column] = *table.spans.get(span.index as usize).ok_or_else(|| {
            format!(
                "span {} of {:?}, whose table has {}",
                span.index,
                span.anchor,
                table.spans.len()
            )
        })?;
        let file =
            table.files.get(file as usize).ok_or("a span in a file its table does not name")?;
        Ok((file.clone(), line, column))
    }
}

impl SpanTable {
    /// `BZSPANS1`, the file count and each file as a length and its UTF-8, the span count and
    /// each span as three little-endian `u32`s.
    fn decode(bytes: &[u8]) -> Result<SpanTable, String> {
        let mut at = bytes.strip_prefix(b"BZSPANS1").ok_or("a span table without its magic")?;
        let count = word(&mut at)?;
        let mut files = Vec::with_capacity((count as usize).min(at.len() / 4));
        for _ in 0..count {
            let len = word(&mut at)? as usize;
            let name = take(&mut at, len)?;
            files.push(
                String::from_utf8(name.to_vec()).map_err(|_| "a file name that is not UTF-8")?,
            );
        }
        let count = word(&mut at)?;
        let mut spans = Vec::with_capacity((count as usize).min(at.len() / 12));
        for _ in 0..count {
            spans.push([word(&mut at)?, word(&mut at)?, word(&mut at)?]);
        }
        Ok(SpanTable { files, spans })
    }
}

fn take<'a>(at: &mut &'a [u8], len: usize) -> Result<&'a [u8], String> {
    if at.len() < len {
        return Err("a truncated span table".to_owned());
    }
    let (head, rest) = at.split_at(len);
    *at = rest;
    Ok(head)
}

fn word(at: &mut &[u8]) -> Result<u32, String> {
    let bytes = take(at, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn val_type(ty: ValType) -> wasm_encoder::ValType {
    match ty {
        ValType::I32 => wasm_encoder::ValType::I32,
        ValType::I64 => wasm_encoder::ValType::I64,
        ValType::F32 => wasm_encoder::ValType::F32,
        ValType::F64 => wasm_encoder::ValType::F64,
        ValType::V128 => wasm_encoder::ValType::V128,
        ValType::FuncRef => wasm_encoder::ValType::Ref(RefType::FUNCREF),
        ValType::ExternRef => wasm_encoder::ValType::Ref(RefType::EXTERNREF),
    }
}

impl Output {
    fn module(self) -> Vec<u8> {
        let mut types = TypeSection::new();
        for ty in &self.types {
            types.ty().function(
                ty.params.iter().map(|&it| val_type(it)),
                ty.results.iter().map(|&it| val_type(it)),
            );
        }
        let mut imports = ImportSection::new();
        let memory = MemoryType {
            minimum: 0,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        };
        imports.import(ENV, "memory", EntityType::Memory(memory));
        let table = TableType {
            element_type: RefType::FUNCREF,
            table64: false,
            minimum: 0,
            maximum: None,
            shared: false,
        };
        imports.import(ENV, "table", EntityType::Table(table));
        let stack_pointer =
            GlobalType { val_type: wasm_encoder::ValType::I32, mutable: true, shared: false };
        imports.import(ENV, "stack_pointer", EntityType::Global(stack_pointer));
        for (module, field, ty) in &self.imports {
            imports.import(module, field, EntityType::Function(*ty));
        }
        let mut functions = FunctionSection::new();
        let mut code = CodeSection::new();
        for (ty, body, _) in &self.defined {
            functions.function(*ty);
            code.raw(body);
        }
        let mut elements = ElementSection::new();
        for &(slot, function) in &self.elements {
            let slot = ConstExpr::i32_const(slot as i32);
            elements.active(None, &slot, Elements::Functions([function][..].into()));
        }
        let mut data = DataSection::new();
        for (address, bytes) in &self.data {
            data.active(0, &ConstExpr::i32_const(*address as i32), bytes.iter().copied());
        }
        let mut module = Module::new();
        module.section(&types).section(&imports).section(&functions).section(&elements);
        module.section(&code).section(&data);
        // For a trap to say where it was.
        let mut names = NameMap::new();
        for (at, (module, field, _)) in self.imports.iter().enumerate() {
            names.append(at as u32, &format!("{module}.{field}"));
        }
        for (at, (.., name)) in self.defined.iter().enumerate() {
            names.append((self.imports.len() + at) as u32, name);
        }
        let mut section = NameSection::new();
        section.functions(&names);
        module.section(&section);
        module.finish()
    }
}
