//! Code made while the program runs, which no image on disk holds: each maker registers a code
//! map ([`register`]), its code's range and the functions in it, so a sampled address in it is
//! named by function ([`name`]). blaze's wasm proc macros are the first: Wasmtime compiles each
//! macro module, and blaze-macro-eval registers the module's `.text` with its functions'
//! offsets as Wasmtime gives them (`Module::functions`).
//!
//! A map keeps what owns its code alive (`keep`, the module itself), so its range never names
//! code that was freed and reused: a map lives as long as the process. blaze keeps every module
//! it loads for the daemon's life anyway (its memo), so this holds nothing it would have let go.
//!
//! The names are only in this process: a sample's frames in JIT code are named when its rows
//! are written, and its raw address (`stack_pcs`) names nothing later.

use std::any::Any;

use blaze_core::lock::ReadLock;

/// A function in a code map, by its offset from the map's start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    pub offset: u64,
    pub len: u64,
    /// Its name, demangled; `None` when the module gives none.
    pub name: Option<String>,
    /// Its index in its module, for a function with no name.
    pub index: u32,
}

/// A stretch of code made while running, and its functions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeMap {
    /// The address of its first byte.
    pub start: u64,
    pub len: u64,
    /// What it is, for a frame no function covers: `wasm module serde_derive`.
    pub module: String,
    /// Its functions, any order: [`register`] sorts them.
    pub functions: Vec<Function>,
}

/// The maps registered, by start, each with what keeps its code alive.
// alloc: once a process
static MAPS: ReadLock<Vec<(CodeMap, Box<dyn Any + Send + Sync>)>> =
    ReadLock::new("ixprof.jit.maps", Vec::new());

/// Registers `map`, keeping `keep` (what owns its code) alive with it for the process's life.
/// A map whose range is already registered (the same code registered twice) is left as it was.
pub fn register(mut map: CodeMap, keep: Box<dyn Any + Send + Sync>) {
    if map.len == 0 {
        return;
    }
    map.functions.sort_unstable_by_key(|it| (it.offset, it.index));
    let mut maps = MAPS.write();
    let at = maps.partition_point(|it| it.0.start < map.start);
    if maps.get(at).is_some_and(|it| it.0.start == map.start) {
        return;
    }
    maps.insert(at, (map, keep));
}

/// The name of the function at `pc` in a registered code map: `<function> [wasm]` for a named
/// one, `wasm function <index> of <module>` for one with no name, `code of <module> outside its
/// functions` between them (Wasmtime's trampolines); `None` outside every map.
pub fn name(pc: u64) -> Option<String> {
    let maps = MAPS.read();
    let at = maps.partition_point(|it| it.0.start <= pc).checked_sub(1)?;
    let map = &maps[at].0;
    let offset = pc.checked_sub(map.start).filter(|it| *it < map.len)?;
    let i = map.functions.partition_point(|it| it.offset <= offset).checked_sub(1);
    let function =
        i.map(|i| &map.functions[i]).filter(|it| offset < it.offset.saturating_add(it.len));
    // alloc: a frame's name, once an address a flush
    Some(match function {
        Some(Function { name: Some(name), .. }) => format!("{name} [wasm]"),
        Some(Function { index, .. }) => format!("wasm function {index} of {}", map.module),
        None => format!("code of {} outside its functions", map.module),
    })
}

/// Whether any code map is registered: a profile says JIT frames are named only when one is.
pub fn any() -> bool {
    !MAPS.read().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_in_a_code_map_is_named_by_its_function() {
        let start = 0x7000_0000_0000;
        register(
            CodeMap {
                start,
                len: 0x300,
                module: "wasm module m".into(),
                functions: vec![
                    Function { offset: 0x100, len: 0x80, name: Some("m::expand".into()), index: 3 },
                    Function { offset: 0x0, len: 0x40, name: None, index: 1 },
                ],
            },
            Box::new(()),
        );
        assert_eq!(name(start + 0x110).as_deref(), Some("m::expand [wasm]"));
        assert_eq!(name(start + 0x10).as_deref(), Some("wasm function 1 of wasm module m"));
        assert_eq!(
            name(start + 0x50).as_deref(),
            Some("code of wasm module m outside its functions")
        );
        assert_eq!(name(start + 0x300), None);
        assert_eq!(name(start - 1), None);
    }
}
