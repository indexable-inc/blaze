# Requests from link-pewasm-wasm

The wasm writer of blaze-link (`crates/blaze-link/src/wasm.rs`, `wasm/`) is written against
these. The blaze-link half (model and core) is in `/tmp/blaze-lanes/v2-link-requests.md`
(W2-W8), for lane `link`.

## 1. `blaze-codegen`: `blaze_codegen::wasm`, what a wasm function is to the linker

Where: a new file `crates/blaze-codegen/src/wasm.rs`, and `pub mod wasm;` in
`crates/blaze-codegen/src/lib.rs` (after `pub mod template;`).

Why: a wasm module needs each function's type, the types its `call_indirect`s name (type
indices the linker assigns, deduplicated across the module), where the body names the
`__stack_pointer` global, and each import's module, field and type. `CompiledFunction` carries
none of it. With this, the `code` of an `Arch::Wasm32` `CompiledFunction` is
`Code::encode(&code, &body).0`: a header, then the body (the locals and the instructions, as
the code section has them after the body's size), and its `relocs` count from the start of
`code`, header included (add `Code::encode`'s second value to offsets counted from the body).
The linker's wasm reader makes the functions of C objects (wasi-libc) the same way, so the
writer reads every code piece one way. It is `hir-ty-codegen/wasm.rs`'s `WasmFunction::encode`
(a postcard header, then the body) made a shared format without dependencies.

Relocations of wasm code, as the linker applies them (5-byte padded LEBs, 4-byte fields in
data):

- `RelocKind::WasmFunction`: the operand of a `call`, the callee's function index.
- `RelocKind::WasmTable`: a function's address (its table slot): the operand of an `i32.const`
  in code, 4 bytes in data (vtables, function pointers in statics).
- `RelocKind::WasmMemory`: an address in memory plus the addend: the operand of an `i32.const`
  in code, 4 bytes in data.

`hir-ty-codegen`'s `CALL` relocations are `WasmFunction`; its `Abs4` ones `WasmTable` when
the target is a function and `WasmMemory` otherwise. Thread-locals on wasm32 (no atomics) are
plain statics, as LLVM makes them: no `TlsOffset`.

```rust
//! What a wasm function is to the linker.
//!
//! The `code` of a [`crate::CompiledFunction`] of [`crate::Arch::Wasm32`] is a [`Code`] and
//! then the body: the locals and the instructions, as the code section has them after the
//! body's size ([`Code::encode`]). Its `relocs` count from the start of `code`. The linker
//! numbers the module's types and globals, so the places in the body that name one are in the
//! header rather than relocations.

use serde::{Deserialize, Serialize};

/// A value type, as its byte in the binary format.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum ValType {
    I32 = 0x7f,
    I64 = 0x7e,
    F32 = 0x7d,
    F64 = 0x7c,
    V128 = 0x7b,
    FuncRef = 0x70,
    ExternRef = 0x6f,
}

impl ValType {
    pub fn from_byte(byte: u8) -> Option<ValType> {
        Some(match byte {
            0x7f => ValType::I32,
            0x7e => ValType::I64,
            0x7d => ValType::F32,
            0x7c => ValType::F64,
            0x7b => ValType::V128,
            0x70 => ValType::FuncRef,
            0x6f => ValType::ExternRef,
            _ => return None,
        })
    }
}

/// A function type.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Signature {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

impl Signature {
    /// As the type section has it: `0x60`, the parameters, the results.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(0x60);
        for values in [&self.params, &self.results] {
            uleb(out, values.len() as u64);
            out.extend(values.iter().map(|&it| it as u8));
        }
    }

    /// Reads what [`Signature::encode`] wrote at `*at`, and moves `*at` past it.
    pub fn decode(bytes: &[u8], at: &mut usize) -> Result<Signature, String> {
        if byte(bytes, at)? != 0x60 {
            return Err("wasm: a type that is not a function type".to_owned());
        }
        let mut lists = [Vec::new(), Vec::new()];
        for list in &mut lists {
            for _ in 0..read_uleb(bytes, at)? {
                let byte = byte(bytes, at)?;
                let ty = ValType::from_byte(byte)
                    .ok_or_else(|| format!("wasm: the value type {byte:#04x}"))?;
                list.push(ty);
            }
        }
        let [params, results] = lists;
        Ok(Signature { params, results })
    }
}

/// What a function the module doesn't define is imported as.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Import {
    pub module: String,
    pub field: String,
    pub ty: Signature,
}

/// What the linker needs of a wasm function beside its body.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Code {
    pub ty: Signature,
    /// The types its `call_indirect`s name.
    pub types: Vec<Signature>,
    /// Where the body has a type index to fill in (a 5-byte padded LEB, from the start of the
    /// body), and which of `types` it is.
    pub type_fixups: Vec<(u32, u32)>,
    /// Where the body has the index of the `__stack_pointer` global to fill in, the same way.
    pub stack_pointer: Vec<u32>,
    /// What each `Target::Extern` its code and its `data` name is imported as when nothing
    /// defines it, by the extern's name.
    pub imports: Vec<(String, Import)>,
    /// The name the module exports it under whatever the output's exports say (C's
    /// `export_name`).
    pub export: Option<String>,
}

impl Code {
    /// The function's `code`: the header's length, the header, then `body`; and where `body`
    /// starts in it.
    pub fn encode(&self, body: &[u8]) -> (Vec<u8>, u32) {
        let mut header = Vec::new();
        self.ty.encode(&mut header);
        uleb(&mut header, self.types.len() as u64);
        for ty in &self.types {
            ty.encode(&mut header);
        }
        uleb(&mut header, self.type_fixups.len() as u64);
        for &(at, which) in &self.type_fixups {
            uleb(&mut header, at.into());
            uleb(&mut header, which.into());
        }
        uleb(&mut header, self.stack_pointer.len() as u64);
        for &at in &self.stack_pointer {
            uleb(&mut header, at.into());
        }
        uleb(&mut header, self.imports.len() as u64);
        for (name, import) in &self.imports {
            string(&mut header, name);
            string(&mut header, &import.module);
            string(&mut header, &import.field);
            import.ty.encode(&mut header);
        }
        match &self.export {
            None => header.push(0),
            Some(name) => {
                header.push(1);
                string(&mut header, name);
            }
        }
        let mut code = Vec::with_capacity(5 + header.len() + body.len());
        uleb(&mut code, header.len() as u64);
        code.extend(header);
        let start = code.len() as u32;
        code.extend(body);
        (code, start)
    }

    /// Reads what [`Code::encode`] wrote; the second value is where the body starts.
    pub fn decode(code: &[u8]) -> Result<(Code, usize), String> {
        let mut at = 0;
        let len = read_uleb(code, &mut at)?;
        let start = usize::try_from(len)
            .ok()
            .and_then(|len| at.checked_add(len))
            .filter(|&end| end <= code.len())
            .ok_or("wasm: a truncated code header")?;
        let header = &code[..start];
        let ty = Signature::decode(header, &mut at)?;
        let mut types = Vec::new();
        for _ in 0..read_uleb(header, &mut at)? {
            types.push(Signature::decode(header, &mut at)?);
        }
        let mut type_fixups = Vec::new();
        for _ in 0..read_uleb(header, &mut at)? {
            let place = read_u32(header, &mut at)?;
            type_fixups.push((place, read_u32(header, &mut at)?));
        }
        let mut stack_pointer = Vec::new();
        for _ in 0..read_uleb(header, &mut at)? {
            stack_pointer.push(read_u32(header, &mut at)?);
        }
        let mut imports = Vec::new();
        for _ in 0..read_uleb(header, &mut at)? {
            let name = read_string(header, &mut at)?;
            let module = read_string(header, &mut at)?;
            let field = read_string(header, &mut at)?;
            let ty = Signature::decode(header, &mut at)?;
            imports.push((name, Import { module, field, ty }));
        }
        let export = match byte(header, &mut at)? {
            0 => None,
            1 => Some(read_string(header, &mut at)?),
            other => return Err(format!("wasm: a code header's export tag {other}")),
        };
        if at != start {
            return Err("wasm: a code header longer than what it holds".to_owned());
        }
        Ok((Code { ty, types, type_fixups, stack_pointer, imports, export }, start))
    }
}

fn uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn string(out: &mut Vec<u8>, string: &str) {
    uleb(out, string.len() as u64);
    out.extend(string.as_bytes());
}

fn byte(bytes: &[u8], at: &mut usize) -> Result<u8, String> {
    let byte = *bytes.get(*at).ok_or("wasm: a truncated code header")?;
    *at += 1;
    Ok(byte)
}

fn read_uleb(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = byte(bytes, at)?;
        if shift >= 64 {
            return Err("wasm: a LEB128 longer than 64 bits".to_owned());
        }
        value |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
}

fn read_u32(bytes: &[u8], at: &mut usize) -> Result<u32, String> {
    u32::try_from(read_uleb(bytes, at)?).map_err(|_| "wasm: a number past 32 bits".to_owned())
}

fn read_string(bytes: &[u8], at: &mut usize) -> Result<String, String> {
    let len = read_uleb(bytes, at)?;
    let end = usize::try_from(len)
        .ok()
        .and_then(|len| at.checked_add(len))
        .filter(|&end| end <= bytes.len())
        .ok_or("wasm: a truncated code header")?;
    let string = std::str::from_utf8(&bytes[*at..end]).map_err(|_| "wasm: a name that is not UTF-8")?;
    *at = end;
    Ok(string.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_round_trips() {
        let code = Code {
            ty: Signature { params: vec![ValType::I32, ValType::I64], results: vec![ValType::F32] },
            types: vec![Signature::default(), Signature { params: vec![ValType::V128], results: vec![] }],
            type_fixups: vec![(3, 1), (200, 0)],
            stack_pointer: vec![9],
            imports: vec![(
                "fd_write".to_owned(),
                Import {
                    module: "wasi_snapshot_preview1".to_owned(),
                    field: "fd_write".to_owned(),
                    ty: Signature { params: vec![ValType::I32; 4], results: vec![ValType::I32] },
                },
            )],
            export: Some("_start".to_owned()),
        };
        let body = [0x00, 0x0b];
        let (bytes, start) = code.encode(&body);
        assert_eq!(&bytes[start as usize..], &body);
        let (back, at) = Code::decode(&bytes).unwrap();
        assert_eq!(at, start as usize);
        assert_eq!(back, code);
        assert!(Code::decode(&bytes[..start as usize - 1]).is_err());
    }

    #[test]
    fn signature_encodes_as_the_type_section() {
        let mut out = Vec::new();
        Signature { params: vec![ValType::I32], results: vec![] }.encode(&mut out);
        assert_eq!(out, [0x60, 1, 0x7f, 0]);
    }
}
```


> forwarded to codegen: `crates/blaze-codegen/` is codegen's and codegen is running; `src/wasm.rs` and `pub mod wasm;` are not there yet.

> applied by codegen (2026-10-29): see link.md 8.

## 2. `Static::imports` (with lane `link`'s `Static` request)

`pub imports: Vec<(String, wasm::Import)>` on `blaze_codegen::Static`: what each
`Target::Extern` its `Init` relocations name is imported as, on wasm (a function pointer to a
`#[link(wasm_import_module)]` function stored in a static). Empty elsewhere.

> forwarded to codegen: `Static::imports` (`crates/blaze-codegen/src/lib.rs`), with link's `Static` request.

> applied by codegen (2026-10-29): see link.md 8.
