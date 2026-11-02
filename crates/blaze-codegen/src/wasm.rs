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
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)] // clone: the link keeps each signature new to the module once
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
            // alloc: the error's message
            return Err("wasm: a type that is not a function type".to_owned());
        }
        let mut read_list = || -> Result<Vec<ValType>, String> {
            let count = read_uleb(bytes, at)?;
            let mut list = Vec::with_capacity(bounded(count, bytes, *at));
            for _ in 0..count {
                let byte = byte(bytes, at)?;
                let ty = ValType::from_byte(byte)
                    // alloc: the error's message
                    .ok_or_else(|| format!("wasm: the value type {byte:#04x}"))?;
                list.push(ty);
            }
            Ok(list)
        };
        let params = read_list()?;
        let results = read_list()?;
        Ok(Signature { params, results })
    }
}

/// What a function the module doesn't define is imported as.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)] // clone: once per import a function or static names, kept by the link
pub struct Import {
    pub module: String,
    pub field: String,
    pub ty: Signature,
}

/// What the linker needs of a wasm function beside its body.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Default)]
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
        // The header is written first, into the buffer the code is returned in, and its length
        // put in front of it once known.
        let mut header = Vec::with_capacity(64 + body.len());
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
        let (len, len_bytes) = uleb_bytes(header.len() as u64);
        let mut code = header;
        code.reserve(len_bytes + body.len());
        code.splice(0..0, len[..len_bytes].iter().copied());
        let start = code.len() as u32;
        code.extend_from_slice(body);
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
        let count = read_uleb(header, &mut at)?;
        let mut types = Vec::with_capacity(bounded(count, header, at));
        for _ in 0..count {
            types.push(Signature::decode(header, &mut at)?);
        }
        let count = read_uleb(header, &mut at)?;
        let mut type_fixups = Vec::with_capacity(bounded(count, header, at));
        for _ in 0..count {
            let place = read_u32(header, &mut at)?;
            type_fixups.push((place, read_u32(header, &mut at)?));
        }
        let count = read_uleb(header, &mut at)?;
        let mut stack_pointer = Vec::with_capacity(bounded(count, header, at));
        for _ in 0..count {
            stack_pointer.push(read_u32(header, &mut at)?);
        }
        let count = read_uleb(header, &mut at)?;
        let mut imports = Vec::with_capacity(bounded(count, header, at));
        for _ in 0..count {
            let name = read_string(header, &mut at)?;
            let module = read_string(header, &mut at)?;
            let field = read_string(header, &mut at)?;
            let ty = Signature::decode(header, &mut at)?;
            imports.push((name, Import { module, field, ty }));
        }
        let export = match byte(header, &mut at)? {
            0 => None,
            1 => Some(read_string(header, &mut at)?),
            // alloc: the error's message
            other => return Err(format!("wasm: a code header's export tag {other}")),
        };
        if at != start {
            // alloc: the error's message
            return Err("wasm: a code header longer than what it holds".to_owned());
        }
        Ok((Code { ty, types, type_fixups, stack_pointer, imports, export }, start))
    }
}

fn uleb(out: &mut Vec<u8>, value: u64) {
    let (bytes, len) = uleb_bytes(value);
    out.extend_from_slice(&bytes[..len]);
}

/// `value` as an unsigned LEB128: its bytes and how many of them there are.
fn uleb_bytes(mut value: u64) -> ([u8; 10], usize) {
    let mut bytes = [0; 10];
    let mut len = 0;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            bytes[len] = byte;
            return (bytes, len + 1);
        }
        bytes[len] = byte | 0x80;
        len += 1;
    }
}

/// The room to make for `count` items read from `bytes` at `at`: each takes at least a byte,
/// so a count past the bytes left is cut to them, and a corrupt count reserves no more.
fn bounded(count: u64, bytes: &[u8], at: usize) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX).min(bytes.len().saturating_sub(at))
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
        if shift >= 64 || (shift == 63 && byte & 0x7e != 0) {
            // alloc: the error's message
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
    // alloc: the error's message
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
    // alloc: an import's name, which `Code` owns; a few per function that imports
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

    #[test]
    fn a_leb_past_64_bits_is_refused() {
        let mut at = 0;
        assert_eq!(read_uleb(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01], &mut at), Ok(u64::MAX));
        let mut at = 0;
        assert!(read_uleb(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02], &mut at).is_err());
    }
}
